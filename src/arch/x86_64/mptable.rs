//! Intel MultiProcessor Specification 1.4 table: how the guest learns about its CPUs,
//! the IOAPIC and interrupt routing without ACPI.
//!
//! Built as a little-endian byte vector (no `unsafe`, no packed structs) and written
//! at `MPTABLE_START` (0x9fc00), the start of the last KiB of base memory, which Linux
//! scans for the `_MP_` floating pointer. e820 already reports that KiB as reserved.

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::{IOAPIC_ADDRESS, MPTABLE_START};
use crate::error::{Error, Result};

/// Local APIC base address every CPU sees.
const LAPIC_ADDRESS: u32 = 0xfee0_0000;
/// MP spec revision 1.4.
const SPEC_REV: u8 = 4;
/// Local APIC version of an integrated xAPIC, as KVM's in-kernel LAPIC reports.
const LAPIC_VERSION: u8 = 0x14;
/// Version of the in-kernel IOAPIC (82093AA-compatible).
const IOAPIC_VERSION: u8 = 0x11;
/// ISA IRQs 0..16 are routed to IOAPIC pins 0..16 (KVM's default GSI routing).
const ISA_IRQS: u8 = 16;
/// The table must fit in the reserved KiB it lives in.
const MAX_SIZE: usize = 1024;

const ENTRY_PROCESSOR: u8 = 0;
const ENTRY_BUS: u8 = 1;
const ENTRY_IOAPIC: u8 = 2;
const ENTRY_IO_INTERRUPT: u8 = 3;
const ENTRY_LOCAL_INTERRUPT: u8 = 4;

const CPU_ENABLED: u8 = 1 << 0;
const CPU_BOOTSTRAP: u8 = 1 << 1;
const IOAPIC_ENABLED: u8 = 1 << 0;
const INT_TYPE_INT: u8 = 0;
const INT_TYPE_NMI: u8 = 1;
const INT_TYPE_EXTINT: u8 = 3;
/// Polarity and trigger mode "conform to the bus": for ISA that is active-high, edge.
const INT_FLAGS_CONFORMING: u16 = 0;
const ISA_BUS_ID: u8 = 0;
/// Destination "all local APICs" in a local interrupt entry.
const ALL_LAPICS: u8 = 0xff;

/// Size of the floating pointer structure.
const FLOATING_POINTER_SIZE: usize = 16;
/// Size of the configuration table header.
const HEADER_SIZE: usize = 44;

/// Byte value that makes the bytes of `bytes` plus it sum to 0 (mod 256).
fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |sum, b| sum.wrapping_add(*b))
        .wrapping_neg()
}

/// Build the MP table for `num_cpus` CPUs with APIC IDs `0..num_cpus` (CPU 0 is the
/// bootstrap processor) and one IOAPIC with ID `num_cpus`. `cpu_signature` and
/// `cpu_features` are CPUID leaf 1 EAX and EDX, copied into each processor entry.
/// Returns the bytes to place at `MPTABLE_START`.
pub fn build(num_cpus: u8, cpu_signature: u32, cpu_features: u32) -> Result<Vec<u8>> {
    let ioapic_id = num_cpus;
    let mut entries: Vec<u8> = Vec::new();
    let mut entry_count: u16 = 0;

    for apic_id in 0..num_cpus {
        let flags = CPU_ENABLED | if apic_id == 0 { CPU_BOOTSTRAP } else { 0 };
        entries.extend_from_slice(&[ENTRY_PROCESSOR, apic_id, LAPIC_VERSION, flags]);
        entries.extend_from_slice(&cpu_signature.to_le_bytes());
        entries.extend_from_slice(&cpu_features.to_le_bytes());
        entries.extend_from_slice(&[0; 8]);
        entry_count += 1;
    }

    entries.extend_from_slice(&[ENTRY_BUS, ISA_BUS_ID]);
    entries.extend_from_slice(b"ISA   ");
    entry_count += 1;

    entries.extend_from_slice(&[ENTRY_IOAPIC, ioapic_id, IOAPIC_VERSION, IOAPIC_ENABLED]);
    entries.extend_from_slice(&(IOAPIC_ADDRESS as u32).to_le_bytes());
    entry_count += 1;

    for irq in 0..ISA_IRQS {
        entries.extend_from_slice(&[ENTRY_IO_INTERRUPT, INT_TYPE_INT]);
        entries.extend_from_slice(&INT_FLAGS_CONFORMING.to_le_bytes());
        entries.extend_from_slice(&[ISA_BUS_ID, irq, ioapic_id, irq]);
        entry_count += 1;
    }

    // The PIC's output reaches the CPUs as ExtINT on LINT0; NMI arrives on LINT1.
    for (int_type, lint) in [(INT_TYPE_EXTINT, 0u8), (INT_TYPE_NMI, 1)] {
        entries.extend_from_slice(&[ENTRY_LOCAL_INTERRUPT, int_type]);
        entries.extend_from_slice(&INT_FLAGS_CONFORMING.to_le_bytes());
        entries.extend_from_slice(&[ISA_BUS_ID, 0, ALL_LAPICS, lint]);
        entry_count += 1;
    }

    let table_len = HEADER_SIZE + entries.len();
    if FLOATING_POINTER_SIZE + table_len > MAX_SIZE {
        return Err(Error::GuestMemory(format!(
            "MP table for {num_cpus} CPUs is {} bytes; at most {MAX_SIZE} fit",
            FLOATING_POINTER_SIZE + table_len
        )));
    }
    let config_addr = MPTABLE_START + FLOATING_POINTER_SIZE as u64;

    let mut table = Vec::with_capacity(table_len);
    table.extend_from_slice(b"PCMP");
    table.extend_from_slice(&(table_len as u16).to_le_bytes());
    table.push(SPEC_REV);
    table.push(0); // checksum, filled in below
    table.extend_from_slice(b"KVMHELLO"); // OEM ID, 8 bytes
    table.extend_from_slice(b"MINIVMM     "); // product ID, 12 bytes
    table.extend_from_slice(&0u32.to_le_bytes()); // no OEM table
    table.extend_from_slice(&0u16.to_le_bytes()); // OEM table size
    table.extend_from_slice(&entry_count.to_le_bytes());
    table.extend_from_slice(&LAPIC_ADDRESS.to_le_bytes());
    table.extend_from_slice(&0u16.to_le_bytes()); // no extended table
    table.push(0); // extended table checksum
    table.push(0); // reserved
    table.extend_from_slice(&entries);
    table[7] = checksum(&table);

    let mut out = Vec::with_capacity(FLOATING_POINTER_SIZE + table_len);
    out.extend_from_slice(b"_MP_");
    out.extend_from_slice(&(config_addr as u32).to_le_bytes());
    out.push(1); // length in 16-byte units
    out.push(SPEC_REV);
    out.push(0); // checksum, filled in below
                 // Feature bytes 1-5 all 0: a configuration table is present, and the system uses
                 // virtual wire mode (no IMCR).
    out.extend_from_slice(&[0; 5]);
    out[10] = checksum(&out);
    out.extend_from_slice(&table);
    Ok(out)
}

/// Build the MP table and write it into guest memory at `MPTABLE_START`.
pub fn write(
    memory: &GuestMemoryMmap,
    num_cpus: u8,
    cpu_signature: u32,
    cpu_features: u32,
) -> Result<()> {
    let bytes = build(num_cpus, cpu_signature, cpu_features)?;
    memory
        .write_slice(&bytes, GuestAddress(MPTABLE_START))
        .map_err(|e| Error::GuestMemory(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0u8, |s, b| s.wrapping_add(*b))
    }

    fn u16_at(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([b[off], b[off + 1]])
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }

    /// Re-parse the table the way Linux's mpparse does and check every field we set.
    #[test]
    fn table_parses_with_valid_checksums_and_entries() {
        for n in 1..=4u8 {
            let bytes = build(n, 0x000b_0671, 0x178b_fbff).expect("fits");
            let fp = &bytes[..16];
            assert_eq!(&fp[..4], b"_MP_");
            assert_eq!(u32_at(fp, 4), (MPTABLE_START + 16) as u32);
            assert_eq!(fp[8], 1);
            assert_eq!(fp[9], 4);
            assert_eq!(sum(fp), 0, "floating pointer checksum");
            assert_eq!(
                MPTABLE_START % 16,
                0,
                "floating pointer is paragraph-aligned"
            );

            let table = &bytes[16..];
            assert_eq!(&table[..4], b"PCMP");
            let len = usize::from(u16_at(table, 4));
            assert_eq!(len, table.len());
            assert_eq!(sum(table), 0, "config table checksum");
            assert_eq!(u32_at(table, 36), 0xfee0_0000);

            let count = usize::from(u16_at(table, 34));
            let (mut cpus, mut buses, mut ioapics, mut ioints, mut lints) = (0, 0, 0, 0, 0);
            let mut off = 44;
            for _ in 0..count {
                match table[off] {
                    ENTRY_PROCESSOR => {
                        let (id, flags) = (table[off + 1], table[off + 3]);
                        assert_eq!(id, cpus as u8, "APIC IDs are 0..n in order");
                        assert_ne!(flags & CPU_ENABLED, 0);
                        assert_eq!(flags & CPU_BOOTSTRAP != 0, id == 0, "only CPU 0 is the BSP");
                        assert_eq!(u32_at(table, off + 4), 0x000b_0671);
                        assert_eq!(u32_at(table, off + 8), 0x178b_fbff);
                        cpus += 1;
                        off += 20;
                    }
                    ENTRY_BUS => {
                        assert_eq!(&table[off + 2..off + 8], b"ISA   ");
                        buses += 1;
                        off += 8;
                    }
                    ENTRY_IOAPIC => {
                        assert_eq!(table[off + 1], n, "IOAPIC ID follows the APIC IDs");
                        assert_eq!(u32_at(table, off + 4), 0xfec0_0000);
                        ioapics += 1;
                        off += 8;
                    }
                    ENTRY_IO_INTERRUPT => {
                        assert_eq!(table[off + 5], ioints as u8, "source IRQ");
                        assert_eq!(table[off + 6], n, "destination IOAPIC");
                        assert_eq!(table[off + 7], ioints as u8, "IRQ i -> pin i");
                        ioints += 1;
                        off += 8;
                    }
                    ENTRY_LOCAL_INTERRUPT => {
                        let (kind, lint) = (table[off + 1], table[off + 7]);
                        assert!(
                            (kind, lint) == (INT_TYPE_EXTINT, 0)
                                || (kind, lint) == (INT_TYPE_NMI, 1)
                        );
                        lints += 1;
                        off += 8;
                    }
                    other => panic!("unknown entry type {other}"),
                }
            }
            assert_eq!(off, len, "entries fill the table exactly");
            assert_eq!(
                (cpus, buses, ioapics, ioints, lints),
                (usize::from(n), 1, 1, 16, 2)
            );
            assert!(bytes.len() <= 1024);
        }
    }
}
