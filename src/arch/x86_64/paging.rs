//! Identity-mapped page tables covering the first 1 GiB with 2 MiB pages.
//!
//! PML4[0] -> PDPT, PDPT[0] -> PD, PD[i] maps guest-virtual `i * 2 MiB` to the same
//! physical address. 2 MiB pages need only three tables, and unlike 1 GiB pages
//! they don't depend on `pdpe1gb` being exposed under nested virtualization.

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::{PDPT_START, PD_START, PML4_START};
use crate::error::{Error, Result};

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
/// In a page-directory entry: this entry maps a 2 MiB page, not a page table.
const PAGE_SIZE: u64 = 1 << 7;
const ENTRIES_PER_TABLE: u64 = 512;
const TWO_MIB: u64 = 2 << 20;

fn write_entry(memory: &GuestMemoryMmap, table: u64, index: u64, value: u64) -> Result<()> {
    memory
        .write_obj(value, GuestAddress(table + index * 8))
        .map_err(|e| Error::GuestMemory(e.to_string()))
}

/// Write the page tables; afterwards CR3 = `PML4_START`.
pub fn setup_identity_map(memory: &GuestMemoryMmap) -> Result<()> {
    write_entry(memory, PML4_START, 0, PDPT_START | PRESENT | WRITABLE)?;
    write_entry(memory, PDPT_START, 0, PD_START | PRESENT | WRITABLE)?;
    for i in 0..ENTRIES_PER_TABLE {
        write_entry(
            memory,
            PD_START,
            i,
            (i * TWO_MIB) | PRESENT | WRITABLE | PAGE_SIZE,
        )?;
    }
    Ok(())
}
