//! x86_64 guest setup and the guest-physical memory layout from the Phase 1 spec.

pub mod regs;

/// Guest-physical address where a raw image is loaded and starts executing.
pub const RAW_IMAGE_START: u64 = 0x1000;
/// End (exclusive) of the raw image region; the zero page starts here.
pub const RAW_IMAGE_END: u64 = 0x7000;
/// Initial stack pointer; the stack grows down from here.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// Guest-physical address of the 3-page region KVM needs for its TSS on Intel.
/// It lies above the 3 GiB RAM cap, so it never overlaps guest RAM.
pub const KVM_TSS_ADDRESS: usize = 0xfffb_d000;
