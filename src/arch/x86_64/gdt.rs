//! Global Descriptor Table for 64-bit long mode.
//!
//! The selectors match the Linux x86 boot protocol (`__BOOT_CS = 0x10`,
//! `__BOOT_DS = 0x18`), so the same GDT serves the raw long-mode guest and Linux.

use kvm_bindings::kvm_segment;

/// Access byte + flags nibble for each descriptor, packed as `0xF0AA`
/// (F = G, D/B, L, AVL flags; AA = access byte).
/// 64-bit code: G=1, L=1; present, DPL 0, code, execute/read, accessed.
const CODE64_FLAGS: u16 = 0xa09b;
/// Data: G=1, D/B=1; present, DPL 0, data, read/write, accessed.
const DATA_FLAGS: u16 = 0xc093;
/// TSS: G=1; present, system descriptor, type 0xB (64-bit TSS, busy).
const TSS_FLAGS: u16 = 0x808b;
/// With G=1 a 20-bit limit of 0xFFFFF covers the whole 4 GiB.
const FLAT_LIMIT: u32 = 0xf_ffff;

pub const CODE_SELECTOR: u16 = 0x10;
pub const DATA_SELECTOR: u16 = 0x18;
pub const TSS_SELECTOR: u16 = 0x20;

/// Pack a segment descriptor from its flags, base and 20-bit limit.
pub fn entry(flags: u16, base: u32, limit: u32) -> u64 {
    let (flags, base, limit) = (u64::from(flags), u64::from(base), u64::from(limit));
    ((base & 0xff00_0000) << 32)       // base[31:24] -> bits 56..63
        | ((flags & 0xf0ff) << 40)     // access byte -> 40..47, flags -> 52..55
        | ((limit & 0x000f_0000) << 32) // limit[19:16] -> 48..51
        | ((base & 0x00ff_ffff) << 16) // base[23:0] -> 16..39
        | (limit & 0x0000_ffff) // limit[15:0] -> 0..15
}

/// The GDT loaded at `GDT_START`: null, null, code64, data, TSS.
pub fn table() -> [u64; 5] {
    [
        0,
        0,
        entry(CODE64_FLAGS, 0, FLAT_LIMIT),
        entry(DATA_FLAGS, 0, FLAT_LIMIT),
        entry(TSS_FLAGS, 0, FLAT_LIMIT),
    ]
}

/// Decode a descriptor into the form KVM wants for a segment register.
/// KVM does not read the GDT when we set segment registers, so the two must agree.
pub fn segment(entry: u64, selector: u16) -> kvm_segment {
    let bit = |n: u32| ((entry >> n) & 1) as u8;
    let base = ((entry >> 16) & 0x00ff_ffff) | ((entry >> 32) & 0xff00_0000);
    let raw_limit = ((entry & 0xffff) | ((entry >> 32) & 0x000f_0000)) as u32;
    let g = bit(55);
    let present = bit(47);
    kvm_segment {
        base,
        // With G=1 the limit counts 4 KiB pages; KVM wants it in bytes.
        limit: if g == 1 {
            (raw_limit << 12) | 0xfff
        } else {
            raw_limit
        },
        selector,
        type_: ((entry >> 40) & 0xf) as u8,
        present,
        dpl: ((entry >> 45) & 0x3) as u8,
        db: bit(54),
        s: bit(44),
        l: bit(53),
        g,
        avl: bit(52),
        unusable: u8::from(present == 0),
        padding: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_segment_is_flat_64_bit() {
        let seg = segment(table()[2], CODE_SELECTOR);
        assert_eq!(seg.base, 0);
        assert_eq!(seg.limit, 0xffff_ffff);
        assert_eq!((seg.l, seg.db, seg.g, seg.present, seg.s), (1, 0, 1, 1, 1));
        assert_eq!(seg.type_, 0xb);
    }

    #[test]
    fn entry_round_trips_base_and_limit() {
        let seg = segment(entry(0x0093, 0x1234_5678, 0x000a_bcde), 0x8);
        assert_eq!(seg.base, 0x1234_5678);
        assert_eq!(seg.limit, 0x000a_bcde);
        assert_eq!(seg.g, 0);
    }
}
