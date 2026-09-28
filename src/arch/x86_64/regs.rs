//! vCPU register setup for the modes the VMM can start a guest in.

use kvm_bindings::kvm_regs;
use kvm_ioctls::VcpuFd;

use super::BOOT_STACK_POINTER;
use crate::error::{Error, Result};

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
