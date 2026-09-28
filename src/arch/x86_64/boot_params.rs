//! The Linux "zero page" (`struct boot_params`): e820 map, command line and initrd location.

use linux_loader::loader::bootparam::{boot_e820_entry, boot_params, setup_header};
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryMmap};

use super::BOOT_PARAMS_START;
use crate::error::{Error, Result};

/// Start of the Extended BIOS Data Area; RAM below it is usable.
const EBDA_START: u64 = 0x9fc00;
/// Everything from here up is ordinary RAM (the "high memory" the kernel loads into).
pub const HIMEM_START: u64 = 0x10_0000;

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

const BOOT_FLAG_MAGIC: u16 = 0xaa55;
/// "HdrS": the setup header is present.
const HEADER_MAGIC: u32 = 0x5372_6448;
/// `type_of_loader` value for a boot loader without an assigned ID.
const LOADER_TYPE_UNDEFINED: u8 = 0xff;

/// `boot_params` wrapped so it can be written to guest memory with `write_obj`.
#[derive(Clone, Copy, Default)]
#[repr(transparent)]
pub struct BootParams(pub boot_params);

// SAFETY: `boot_params` is a bindgen-generated `#[repr(C, packed)]` struct made only of
// integers and arrays of integers: it has no padding, no pointers or references, and
// every bit pattern is a valid value. `repr(transparent)` keeps the same layout.
unsafe impl ByteValued for BootParams {}

/// Where the initrd was placed in guest memory.
#[derive(Debug, Clone, Copy)]
pub struct InitrdLocation {
    pub addr: u64,
    pub size: u64,
}

/// Build the zero page from the kernel's own setup header, then fill in what the
/// boot loader is responsible for.
pub fn build(
    header: setup_header,
    cmdline_addr: u64,
    initrd: Option<InitrdLocation>,
    mem_size: u64,
) -> Result<BootParams> {
    let mut params = boot_params {
        hdr: header,
        ..Default::default()
    };
    params.hdr.boot_flag = BOOT_FLAG_MAGIC;
    params.hdr.header = HEADER_MAGIC;
    params.hdr.type_of_loader = LOADER_TYPE_UNDEFINED;
    params.hdr.cmd_line_ptr = to_u32(cmdline_addr, "command line address")?;
    if let Some(initrd) = initrd {
        params.hdr.ramdisk_image = to_u32(initrd.addr, "initrd address")?;
        params.hdr.ramdisk_size = to_u32(initrd.size, "initrd size")?;
    }

    let e820 = [
        (0, EBDA_START, E820_RAM),
        (EBDA_START, HIMEM_START - EBDA_START, E820_RESERVED),
        (HIMEM_START, mem_size.saturating_sub(HIMEM_START), E820_RAM),
    ];
    for (i, &(addr, size, kind)) in e820.iter().enumerate() {
        params.e820_table[i] = boot_e820_entry {
            addr,
            size,
            r#type: kind,
        };
    }
    params.e820_entries = e820.len() as u8;
    Ok(BootParams(params))
}

fn to_u32(value: u64, what: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::GuestMemory(format!("{what} {value:#x} exceeds 4 GiB")))
}

/// Write the zero page to `BOOT_PARAMS_START`.
pub fn write(memory: &GuestMemoryMmap, params: &BootParams) -> Result<()> {
    memory
        .write_obj(*params, GuestAddress(BOOT_PARAMS_START))
        .map_err(|e| Error::GuestMemory(e.to_string()))
}
