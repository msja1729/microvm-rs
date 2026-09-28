//! Loading guest images into guest memory.

use std::path::Path;

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

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
