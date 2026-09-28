//! Loading guest images into guest memory.

use std::fs::File;
use std::path::Path;

use linux_loader::loader::bootparam::setup_header;
use linux_loader::loader::{BzImage, KernelLoader};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::arch::x86_64::boot_params::{InitrdLocation, HIMEM_START};
use crate::arch::x86_64::{RAW_IMAGE_END, RAW_IMAGE_START};
use crate::error::{Error, Result};

/// Largest raw image that fits between its load address and the zero page.
pub const MAX_RAW_IMAGE_SIZE: usize = (RAW_IMAGE_END - RAW_IMAGE_START) as usize;

/// Copy the flat binary at `path` to guest-physical `RAW_IMAGE_START`.
/// Returns the entry point (the image's first byte).
pub fn load_raw_image(memory: &GuestMemoryMmap, path: &Path) -> Result<u64> {
    let image = std::fs::read(path).map_err(|source| Error::ImageRead {
        path: path.to_path_buf(),
        source,
    })?;
    if image.is_empty() || image.len() > MAX_RAW_IMAGE_SIZE {
        return Err(Error::ImageSize {
            path: path.to_path_buf(),
            size: image.len(),
            max: MAX_RAW_IMAGE_SIZE,
        });
    }
    memory
        .write_slice(&image, GuestAddress(RAW_IMAGE_START))
        .map_err(|e| Error::GuestMemory(e.to_string()))?;
    Ok(RAW_IMAGE_START)
}

/// `xloadflags` bit: the kernel has a 64-bit entry point at load address + 0x200.
const XLF_KERNEL_64: u16 = 1 << 0;
/// Offset of `startup_64` from the protected-mode kernel's load address.
const STARTUP_64_OFFSET: u64 = 0x200;
/// `initrd_addr_max` for boot protocols older than 2.03, which don't report it.
const DEFAULT_INITRD_ADDR_MAX: u64 = 0x37ff_ffff;
const PAGE_SIZE: u64 = 0x1000;

/// A bzImage kernel loaded into guest memory.
pub struct LoadedKernel {
    /// 64-bit entry point (`startup_64`).
    pub entry: u64,
    /// First address above everything the kernel may use while decompressing itself.
    pub end: u64,
    /// The kernel's setup header, the starting point for `boot_params`.
    pub header: setup_header,
}

/// Load the protected-mode part of a bzImage at `HIMEM_START` (1 MiB).
pub fn load_kernel(memory: &GuestMemoryMmap, path: &Path) -> Result<LoadedKernel> {
    let load_error = |reason: String| Error::KernelLoad {
        path: path.to_path_buf(),
        reason,
    };
    let mut file = File::open(path).map_err(|source| Error::KernelRead {
        path: path.to_path_buf(),
        source,
    })?;
    let result = BzImage::load(memory, None, &mut file, Some(GuestAddress(HIMEM_START)))
        .map_err(|e| load_error(e.to_string()))?;
    let header = result
        .setup_header
        .ok_or_else(|| load_error("no setup header".into()))?;
    let xloadflags = header.xloadflags;
    if xloadflags & XLF_KERNEL_64 == 0 {
        return Err(load_error("kernel has no 64-bit entry point".into()));
    }
    // The kernel decompresses in place and needs `init_size` bytes from its load
    // address; nothing else (e.g. the initrd) may be placed there.
    let init_end = result
        .kernel_load
        .0
        .saturating_add(u64::from(header.init_size));
    Ok(LoadedKernel {
        entry: result.kernel_load.0 + STARTUP_64_OFFSET,
        end: init_end.max(result.kernel_end),
        header,
    })
}

/// Place the initrd as high in RAM as the kernel allows, page-aligned, above the kernel.
pub fn load_initrd(
    memory: &GuestMemoryMmap,
    path: &Path,
    kernel: &LoadedKernel,
    mem_size: u64,
) -> Result<InitrdLocation> {
    let data = std::fs::read(path).map_err(|source| Error::InitrdRead {
        path: path.to_path_buf(),
        source,
    })?;
    let size = data.len() as u64;
    let too_large = || Error::InitrdTooLarge {
        path: path.to_path_buf(),
        size,
    };

    let addr_max = match kernel.header.initrd_addr_max {
        0 => DEFAULT_INITRD_ADDR_MAX,
        max => u64::from(max),
    };
    // The initrd's last byte must be at or below `addr_max` and inside RAM.
    let top = mem_size.min(addr_max.saturating_add(1));
    let addr = top.checked_sub(size).ok_or_else(too_large)? & !(PAGE_SIZE - 1);
    if addr < kernel.end {
        return Err(too_large());
    }
    memory
        .write_slice(&data, GuestAddress(addr))
        .map_err(|e| Error::GuestMemory(e.to_string()))?;
    Ok(InitrdLocation { addr, size })
}
