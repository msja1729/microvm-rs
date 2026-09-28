//! x86_64 guest setup and the guest-physical memory layout from the Phase 1 spec.

pub mod boot_params;
pub mod gdt;
pub mod paging;
pub mod regs;

/// Guest-physical address of the GDT.
pub const GDT_START: u64 = 0x500;
/// Guest-physical address of the (empty) IDT.
pub const IDT_START: u64 = 0x520;
/// Guest-physical address where a raw image is loaded and starts executing.
pub const RAW_IMAGE_START: u64 = 0x1000;
/// End (exclusive) of the raw image region; the zero page starts here.
pub const RAW_IMAGE_END: u64 = 0x7000;
/// Guest-physical address of the Linux zero page (`boot_params`).
pub const BOOT_PARAMS_START: u64 = 0x7000;
/// Initial stack pointer; the stack grows down from here.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// Guest-physical addresses of the identity-mapping page tables.
pub const PML4_START: u64 = 0x9000;
pub const PDPT_START: u64 = 0xa000;
pub const PD_START: u64 = 0xb000;
/// Guest-physical address of the kernel command line.
pub const CMDLINE_START: u64 = 0x2_0000;
/// Kernel command-line buffer size, including the terminating NUL (Linux COMMAND_LINE_SIZE).
pub const CMDLINE_MAX_SIZE: usize = 2048;
/// Guest-physical address of the 3-page region KVM needs for its TSS on Intel.
/// It lies above the 3 GiB RAM cap, so it never overlaps guest RAM.
pub const KVM_TSS_ADDRESS: usize = 0xfffb_d000;
