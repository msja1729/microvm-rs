//! vCPU register setup for the modes the VMM can start a guest in.

use kvm_bindings::{kvm_cpuid_entry2, kvm_lapic_state, kvm_regs, CpuId, KVM_MAX_CPUID_ENTRIES};
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

/// The CPUID table KVM supports on this host, fetched once and specialized per vCPU.
pub fn supported_cpuid(kvm: &Kvm) -> Result<CpuId> {
    // KVM_GET_SUPPORTED_CPUID: every leaf KVM can virtualize on this host.
    kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|source| Error::Kvm {
            op: "KVM_GET_SUPPORTED_CPUID",
            source,
        })
}

/// CPUID leaf 1 EDX: hyper-threading / multi-core ("max logical processors" valid).
const CPUID_1_EDX_HTT: u32 = 1 << 28;

/// Rewrite the topology leaves of `entries` for vCPU `index` of `count`, describing one
/// socket with `count` cores, one thread per core, and APIC ID = vCPU index. The host
/// values of these leaves describe the host's topology, which the guest must not see.
/// Only Intel's leaves are rewritten (AMD's 0x8000_001E etc. are left as reported).
pub fn set_cpuid_topology(entries: &mut [kvm_cpuid_entry2], index: u8, count: u8) {
    let apic_id = u32::from(index);
    let n = u32::from(count.max(1));
    // Bits needed to number the cores: ceil(log2(n)).
    let core_shift = u32::BITS - (n - 1).leading_zeros();
    for entry in entries.iter_mut() {
        match entry.function {
            1 => {
                entry.ebx = (entry.ebx & 0x0000_ffff) | (apic_id << 24) | (n << 16);
                if n > 1 {
                    entry.edx |= CPUID_1_EDX_HTT;
                } else {
                    entry.edx &= !CPUID_1_EDX_HTT;
                }
            }
            4 => {
                let cache_type = entry.eax & 0x1f;
                if cache_type == 0 {
                    continue; // no more caches
                }
                let level = (entry.eax >> 5) & 0x7;
                // EAX[25:14]: logical processors sharing this cache - 1 (only the
                // L3 is shared between cores); EAX[31:26]: cores per package - 1.
                let sharing = if level >= 3 { n - 1 } else { 0 };
                entry.eax = (entry.eax & 0x3fff) | (sharing << 14) | ((n - 1) << 26);
            }
            0xb | 0x1f => {
                let (shift, logical, level_type) = match entry.index {
                    0 => (0, 1, 1),          // SMT level: one thread per core
                    1 => (core_shift, n, 2), // core level: n cores
                    _ => (0, 0, 0),          // no further levels
                };
                entry.eax = shift;
                entry.ebx = logical;
                entry.ecx = (level_type << 8) | (entry.index & 0xff);
                entry.edx = apic_id;
            }
            _ => {}
        }
    }
}

/// Give vCPU `index` of `count` the host's supported CPUID table with its own topology.
/// Without CPUID the guest sees no CPU features, and KVM refuses to enable long mode
/// (EFER.LME requires the guest's CPUID to advertise LM).
pub fn setup_cpuid(supported: &CpuId, vcpu: &VcpuFd, index: u8, count: u8) -> Result<()> {
    let mut cpuid = supported.clone();
    set_cpuid_topology(cpuid.as_mut_slice(), index, count);
    // KVM_SET_CPUID2: what the guest's CPUID instruction will report.
    vcpu.set_cpuid2(&cpuid).map_err(|source| Error::Kvm {
        op: "KVM_SET_CPUID2",
        source,
    })
}

/// CPUID leaf 1 (EAX = signature, EDX = features) from a CPUID table, for the MP table.
pub fn cpu_signature(cpuid: &CpuId) -> (u32, u32) {
    cpuid
        .as_slice()
        .iter()
        .find(|e| e.function == 1)
        .map_or((0, 0), |e| (e.eax, e.edx))
}

/// Offsets of the local vector table entries for LINT0 and LINT1 in the LAPIC page.
const APIC_LVT_LINT0: usize = 0x350;
const APIC_LVT_LINT1: usize = 0x360;
/// Delivery-mode field of an LVT entry (bits 10:8).
const APIC_DELIVERY_MODE_MASK: u32 = 0x700;
const APIC_MODE_EXTINT: u32 = 0x7 << 8;
const APIC_MODE_NMI: u32 = 0x4 << 8;

fn lapic_reg(lapic: &kvm_lapic_state, offset: usize) -> u32 {
    let mut b = [0u8; 4];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = lapic.regs.get(offset + i).map_or(0, |&v| v as u8);
    }
    u32::from_le_bytes(b)
}

fn set_lapic_reg(lapic: &mut kvm_lapic_state, offset: usize, value: u32) {
    for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
        if let Some(reg) = lapic.regs.get_mut(offset + i) {
            *reg = byte as _;
        }
    }
}

/// Program LINT0 as ExtINT (the PIC's output, as the MP table says) and LINT1 as NMI,
/// keeping every other LVT field (mask, trigger) as KVM set it.
pub fn set_lint_modes(lapic: &mut kvm_lapic_state) {
    for (offset, mode) in [
        (APIC_LVT_LINT0, APIC_MODE_EXTINT),
        (APIC_LVT_LINT1, APIC_MODE_NMI),
    ] {
        let value = lapic_reg(lapic, offset);
        set_lapic_reg(lapic, offset, (value & !APIC_DELIVERY_MODE_MASK) | mode);
    }
}

/// Configure the vCPU's in-kernel LAPIC LINT0/LINT1 (see `set_lint_modes`).
pub fn setup_lapic(vcpu: &VcpuFd) -> Result<()> {
    // KVM_GET_LAPIC: read the in-kernel LAPIC's register page.
    let mut lapic = vcpu.get_lapic().map_err(|source| Error::Kvm {
        op: "KVM_GET_LAPIC",
        source,
    })?;
    set_lint_modes(&mut lapic);
    // KVM_SET_LAPIC: write it back with the new LVT entries.
    vcpu.set_lapic(&lapic).map_err(|source| Error::Kvm {
        op: "KVM_SET_LAPIC",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        function: u32,
        index: u32,
        eax: u32,
        ebx: u32,
        ecx: u32,
        edx: u32,
    ) -> kvm_cpuid_entry2 {
        kvm_cpuid_entry2 {
            function,
            index,
            eax,
            ebx,
            ecx,
            edx,
            ..Default::default()
        }
    }

    /// A host-like table: leaf 1, four leaf-4 caches (L1d, L1i, L2, L3) plus the
    /// terminator, leaf 0xB with host values for 3 subleaves, and an unrelated leaf.
    fn host_table() -> Vec<kvm_cpuid_entry2> {
        vec![
            entry(1, 0, 0x000b_0671, 0x1f10_0800, 0x7ffa_fbff, 0x078b_fbff),
            entry(4, 0, 0xfc00_4121, 0, 0, 0),
            entry(4, 1, 0xfc00_4122, 0, 0, 0),
            entry(4, 2, 0xfc01_c143, 0, 0, 0),
            entry(4, 3, 0xfc3f_c163, 0, 0, 0),
            entry(4, 4, 0, 0, 0, 0),
            entry(0xb, 0, 1, 2, 0x100, 31),
            entry(0xb, 1, 7, 32, 0x201, 31),
            entry(0xb, 2, 0, 0, 2, 31),
            entry(7, 0, 0, 0xdead_beef, 0, 0),
        ]
    }

    #[test]
    fn topology_for_vcpu_2_of_4() {
        let mut t = host_table();
        set_cpuid_topology(&mut t, 2, 4);
        assert_eq!(t[0].ebx >> 24, 2, "initial APIC ID");
        assert_eq!((t[0].ebx >> 16) & 0xff, 4, "logical processors");
        assert_eq!(t[0].ebx & 0xffff, 0x0800, "other EBX fields kept");
        assert_ne!(t[0].edx & CPUID_1_EDX_HTT, 0);
        for cache in &t[1..5] {
            assert_eq!(cache.eax >> 26, 3, "cores per package - 1");
        }
        assert_eq!((t[1].eax >> 14) & 0xfff, 0, "L1d private");
        assert_eq!((t[3].eax >> 14) & 0xfff, 0, "L2 private");
        assert_eq!((t[4].eax >> 14) & 0xfff, 3, "L3 shared by 4");
        assert_eq!(t[4].eax & 0x3fff, 0x0163 & 0x3fff);
        assert_eq!(t[5].eax, 0, "terminator untouched");
        assert_eq!((t[6].eax, t[6].ebx, t[6].ecx, t[6].edx), (0, 1, 0x100, 2));
        assert_eq!((t[7].eax, t[7].ebx, t[7].ecx, t[7].edx), (2, 4, 0x201, 2));
        assert_eq!((t[8].eax, t[8].ebx, t[8].ecx, t[8].edx), (0, 0, 2, 2));
        assert_eq!(t[9].ebx, 0xdead_beef, "other leaves untouched");
    }

    #[test]
    fn topology_core_shift_and_single_cpu() {
        for (n, shift) in [(1u8, 0u32), (2, 1), (3, 2), (4, 2)] {
            let mut t = host_table();
            set_cpuid_topology(&mut t, 0, n);
            assert_eq!(t[7].eax, shift, "ceil(log2({n}))");
            assert_eq!(t[7].ebx, u32::from(n));
        }
        let mut t = host_table();
        set_cpuid_topology(&mut t, 0, 1);
        assert_eq!(t[0].edx & CPUID_1_EDX_HTT, 0, "no HTT with one CPU");
        assert_eq!(t[4].eax >> 26, 0);
    }

    #[test]
    fn lint_modes() {
        let mut lapic = kvm_lapic_state::default();
        // Masked (bit 16), level-triggered (bit 15), some old mode and vector.
        set_lapic_reg(&mut lapic, APIC_LVT_LINT0, 0x0001_8200 | 0x31);
        set_lapic_reg(&mut lapic, APIC_LVT_LINT1, 0x0001_0000);
        set_lapic_reg(&mut lapic, 0x370, 0x1234);
        set_lint_modes(&mut lapic);
        assert_eq!(lapic_reg(&lapic, APIC_LVT_LINT0), 0x0001_8700 | 0x31);
        assert_eq!(lapic_reg(&lapic, APIC_LVT_LINT1), 0x0001_0400);
        assert_eq!(
            lapic_reg(&lapic, 0x370),
            0x1234,
            "neighbouring register untouched"
        );
    }
}
