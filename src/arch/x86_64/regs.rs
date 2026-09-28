//! vCPU register setup for the modes the VMM can start a guest in.

use kvm_bindings::{kvm_regs, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuFd};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::gdt::{self, CODE_SELECTOR, DATA_SELECTOR, TSS_SELECTOR};
use super::{paging, BOOT_STACK_POINTER, GDT_START, IDT_START, PML4_START};
use crate::error::{Error, Result};

const CR0_PE: u64 = 1 << 0;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

/// RFLAGS bit 1 is reserved and must always be set; everything else (including IF) is clear.
const RFLAGS_RESERVED: u64 = 0x2;

/// Put the vCPU in 16-bit real mode with every segment selector and base at 0,
/// so that guest-physical address == IP, and start executing at `entry`.
pub fn setup_real_mode(vcpu: &VcpuFd, entry: u64) -> Result<()> {
    // KVM_GET_SREGS: read the reset-state segment/control registers so we only change
    // the fields we care about (CR0.PE stays clear, so the vCPU remains in real mode).
    let mut sregs = vcpu.get_sregs().map_err(|source| Error::Kvm {
        op: "KVM_GET_SREGS",
        source,
    })?;
    // At reset CS is 0xF000 with base 0xFFFF0000 (the BIOS reset vector); we have no
    // BIOS, so flatten every segment to base 0.
    for seg in [
        &mut sregs.cs,
        &mut sregs.ds,
        &mut sregs.es,
        &mut sregs.fs,
        &mut sregs.gs,
        &mut sregs.ss,
    ] {
        seg.base = 0;
        seg.selector = 0;
    }
    // KVM_SET_SREGS: install the flattened segments.
    vcpu.set_sregs(&sregs).map_err(|source| Error::Kvm {
        op: "KVM_SET_SREGS",
        source,
    })?;

    let regs = kvm_regs {
        rip: entry,
        rsp: BOOT_STACK_POINTER,
        rflags: RFLAGS_RESERVED,
        ..Default::default()
    };
    // KVM_SET_REGS: set the instruction pointer, stack pointer and flags.
    vcpu.set_regs(&regs).map_err(|source| Error::Kvm {
        op: "KVM_SET_REGS",
        source,
    })
}

/// Give the vCPU the CPUID table KVM supports on this host. Without it the guest
/// sees no CPU features, and KVM refuses to enable long mode (EFER.LME requires
/// the guest's CPUID to advertise LM).
pub fn setup_cpuid(kvm: &Kvm, vcpu: &VcpuFd) -> Result<()> {
    // KVM_GET_SUPPORTED_CPUID: every leaf KVM can virtualize on this host.
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|source| Error::Kvm {
            op: "KVM_GET_SUPPORTED_CPUID",
            source,
        })?;
    // KVM_SET_CPUID2: what the guest's CPUID instruction will report.
    vcpu.set_cpuid2(&cpuid).map_err(|source| Error::Kvm {
        op: "KVM_SET_CPUID2",
        source,
    })
}

/// Put the vCPU directly in 64-bit long mode: write the GDT, an empty IDT and the
/// identity-mapping page tables into guest memory, then load the matching special
/// registers and start at `entry` with interrupts disabled. `rsi` is handed to the
/// guest in RSI (the Linux 64-bit boot protocol puts the `boot_params` address there).
pub fn setup_long_mode(
    vcpu: &VcpuFd,
    memory: &GuestMemoryMmap,
    entry: u64,
    rsi: u64,
) -> Result<()> {
    let table = gdt::table();
    for (i, descriptor) in table.iter().enumerate() {
        memory
            .write_obj(*descriptor, GuestAddress(GDT_START + 8 * i as u64))
            .map_err(|e| Error::GuestMemory(e.to_string()))?;
    }
    memory
        .write_obj(0u64, GuestAddress(IDT_START))
        .map_err(|e| Error::GuestMemory(e.to_string()))?;
    paging::setup_identity_map(memory)?;

    // KVM_GET_SREGS: start from the reset state and change only what long mode needs.
    let mut sregs = vcpu.get_sregs().map_err(|source| Error::Kvm {
        op: "KVM_GET_SREGS",
        source,
    })?;
    sregs.gdt.base = GDT_START;
    sregs.gdt.limit = (std::mem::size_of_val(&table) - 1) as u16;
    // Limit 0: interrupts stay disabled until the guest installs its own IDT.
    sregs.idt.base = IDT_START;
    sregs.idt.limit = 0;

    let code = gdt::segment(table[usize::from(CODE_SELECTOR / 8)], CODE_SELECTOR);
    let data = gdt::segment(table[usize::from(DATA_SELECTOR / 8)], DATA_SELECTOR);
    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = gdt::segment(table[usize::from(TSS_SELECTOR / 8)], TSS_SELECTOR);

    // Paging with PAE page tables, enabled together with EFER.LME, is what makes
    // this long mode. KVM accepts LMA set directly, as if the switch just happened.
    sregs.cr3 = PML4_START;
    sregs.cr4 |= CR4_PAE;
    sregs.cr0 |= CR0_PE | CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;
    // KVM_SET_SREGS: load the segments, descriptor tables and control registers.
    vcpu.set_sregs(&sregs).map_err(|source| Error::Kvm {
        op: "KVM_SET_SREGS",
        source,
    })?;

    let regs = kvm_regs {
        rip: entry,
        rsp: BOOT_STACK_POINTER,
        rsi,
        rflags: RFLAGS_RESERVED,
        ..Default::default()
    };
    // KVM_SET_REGS: set the instruction pointer, stack pointer and flags.
    vcpu.set_regs(&regs).map_err(|source| Error::Kvm {
        op: "KVM_SET_REGS",
        source,
    })
}
