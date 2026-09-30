//! Kani proof harnesses for the guest-facing virtio code (AC-8.2).
//!
//! Bounds (spec Q5): queue size 4, so chains of at most 4 descriptors, loop unwinding
//! 5. Guest memory is `KaniMem`: a symbolic size where every in-bounds read returns
//! arbitrary bytes. That covers every value a guest could put in memory, including a
//! guest changing it between two reads. Descriptor addresses, lengths, flags and next
//! indices, ring positions and the disk capacity are all symbolic.
//!
//! `KaniMem` can also check where the code reads: `bounds` makes every access outside
//! a given range (e.g. the descriptor table) a proof failure, and it records which
//! descriptor indices were read, and whether any carried `INDIRECT`.

use std::cell::{Cell, RefCell};

use virtio_bindings::virtio_blk::{VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT};
use virtio_bindings::virtio_ring::VRING_DESC_F_INDIRECT;

use super::parse::{
    parse_blk_request, parse_net_tx, zero_vnet_header, BlkRequestType, MAX_TX_LEN, NET_HDR_SIZE,
    SECTOR_SIZE,
};
use super::queue::{range_in_memory, walk_chain, ChainDesc, GuestMem, MemError, Queue};

/// Queue size used by every harness.
const QSIZE: u16 = 4;
/// Size of a descriptor-table entry.
const DESC_SIZE: u64 = 16;

/// Symbolic guest memory.
struct KaniMem {
    size: u64,
    /// Every access must lie in `[base, base + len)`; checked in `u128`.
    bounds: Option<(u64, u64)>,
    /// A read of up to 16 bytes at exactly this address returns these bytes (so a
    /// harness can talk about the value the code saw); all other reads are arbitrary.
    pinned: Option<(u64, [u8; 16])>,
    /// Descriptor-table base: 16-byte reads are recorded as descriptor accesses.
    desc_table: Option<u64>,
    /// Descriptor indices read, in order, and how many. A fixed array (not a `Vec`)
    /// keeps every loop over it concretely bounded for the unwinder.
    indices: RefCell<[u64; QSIZE as usize]>,
    reads: Cell<usize>,
    saw_indirect: Cell<bool>,
    /// Stable descriptors: a descriptor read twice returns the same bytes (a guest that
    /// does not rewrite its descriptor table during the walk). Otherwise every read is
    /// independent, modelling a guest changing descriptors concurrently.
    stable: bool,
    seen: RefCell<[Option<[u8; 16]>; QSIZE as usize]>,
}

impl KaniMem {
    fn new(size: u64) -> Self {
        Self {
            size,
            bounds: None,
            pinned: None,
            desc_table: None,
            indices: RefCell::new([0; QSIZE as usize]),
            reads: Cell::new(0),
            stable: false,
            seen: RefCell::new([None; QSIZE as usize]),
            saw_indirect: Cell::new(false),
        }
    }

    fn check_bounds(&self, addr: u64, len: usize) {
        if let Some((base, blen)) = self.bounds {
            let (addr, len, base, blen) = (addr as u128, len as u128, base as u128, blen as u128);
            assert!(
                addr >= base && addr + len <= base + blen,
                "access outside the ring"
            );
        }
    }
}

impl GuestMem for KaniMem {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), MemError> {
        // Bounds first: even an attempt outside the allowed range is a failure.
        self.check_bounds(addr, buf.len());
        if !range_in_memory(self.size, addr, buf.len() as u64) {
            return Err(MemError);
        }
        // The harnesses only ever read 2- or 16-byte structures.
        assert!(buf.len() <= 16, "unexpected read size");
        let mut bytes: [u8; 16] = match self.pinned {
            Some((pinned_addr, bytes)) if pinned_addr == addr => bytes,
            _ => kani::any(),
        };
        if let (true, Some(table), 16) = (self.stable, self.desc_table, buf.len()) {
            let index = ((addr - table) / DESC_SIZE) as usize;
            if let Some(slot) = self.seen.borrow_mut().get_mut(index) {
                bytes = *slot.get_or_insert(bytes);
            }
        }
        buf.copy_from_slice(&bytes[..buf.len()]);
        if let (Some(table), 16) = (self.desc_table, buf.len()) {
            let offset = addr - table;
            assert!(offset % DESC_SIZE == 0, "misaligned descriptor read");
            let n = self.reads.get();
            assert!(
                n < usize::from(QSIZE),
                "more descriptor reads than the queue size"
            );
            self.indices.borrow_mut()[n] = offset / DESC_SIZE;
            self.reads.set(n + 1);
            let flags = u16::from_le_bytes([buf[12], buf[13]]);
            if u32::from(flags) & VRING_DESC_F_INDIRECT != 0 {
                self.saw_indirect.set(true);
            }
        }
        Ok(())
    }

    fn write(&self, addr: u64, buf: &[u8]) -> Result<(), MemError> {
        self.check_bounds(addr, buf.len());
        if !range_in_memory(self.size, addr, buf.len() as u64) {
            return Err(MemError);
        }
        Ok(())
    }
}

fn any_desc() -> ChainDesc {
    ChainDesc {
        addr: kani::any(),
        len: kani::any(),
        writable: kani::any(),
    }
}

/// Run `check` on a chain of every length `0..=QSIZE` with fully symbolic descriptors.
/// Each branch passes a slice of concrete length, so the solver never has to reason
/// about heap data of symbolic length (with one symbolic-length `Vec`, `net_tx_bounds`
/// alone ran past 15 minutes); the coverage is the same.
fn for_any_chain(check: impl FnOnce(&[ChainDesc])) {
    let all = [any_desc(), any_desc(), any_desc(), any_desc()];
    let n: u8 = kani::any();
    match n {
        0 => check(&all[..0]),
        1 => check(&all[..1]),
        2 => check(&all[..2]),
        3 => check(&all[..3]),
        _ => check(&all[..4]),
    }
}

fn in_memory(size: u64, addr: u64, len: u64) -> bool {
    addr as u128 + len as u128 <= size as u128
}

/// (e) `pop` never panics and reads only `avail.idx` and ring slots; a returned head is
/// `< size`; an `avail.idx` more than `size` ahead is rejected as broken. `add_used`
/// writes only inside the used ring.
#[kani::proof]
#[kani::unwind(5)]
fn pop_stays_in_ring() {
    let mut queue = Queue::default();
    queue.size = QSIZE;
    queue.ready = true;
    queue.desc_table = kani::any();
    queue.avail_ring = kani::any();
    queue.used_ring = kani::any();
    let next_avail: u16 = kani::any();
    queue.set_positions(next_avail, kani::any());
    let avail_idx: u16 = kani::any();
    let mut pinned = [0u8; 16];
    pinned[..2].copy_from_slice(&avail_idx.to_le_bytes());

    let mut mem = KaniMem::new(kani::any());
    mem.bounds = Some((queue.avail_ring, 4 + 2 * u64::from(QSIZE)));
    mem.pinned = Some((queue.avail_ring.wrapping_add(2), pinned));
    let result = queue.pop(&mem);

    let pending = avail_idx.wrapping_sub(next_avail);
    if pending > QSIZE {
        assert!(result.is_err(), "corrupt avail.idx accepted");
    }
    if let Ok(Some(head)) = result {
        assert!(head < QSIZE);
        assert!(pending >= 1 && pending <= QSIZE);
        assert_eq!(queue.next_avail(), next_avail.wrapping_add(1));

        let mut used_mem = KaniMem::new(mem.size);
        used_mem.bounds = Some((queue.used_ring, 4 + 8 * u64::from(QSIZE)));
        let _ = queue.add_used(&used_mem, head, kani::any());
    }
    if let Ok(None) = result {
        assert_eq!(pending, 0);
    }
}

/// (d) + (e) For any descriptor contents, even ones the guest rewrites during the walk:
/// the walk terminates within `size` iterations (the unwind bound proves it), reads
/// only descriptor-table entries with index `< size`, returns 1..=size descriptors, and
/// rejects any chain with an `INDIRECT` descriptor. When the descriptors are stable,
/// a chain that revisits an index (a loop) is rejected. (With a concurrently rewritten
/// table a revisit can end in a valid, bounded chain; see notes/phase-2.md.)
#[kani::proof]
#[kani::unwind(5)]
fn walk_chain_bounded() {
    let desc_table: u64 = kani::any();
    let head: u16 = kani::any();
    let mut mem = KaniMem::new(kani::any());
    mem.bounds = Some((desc_table, DESC_SIZE * u64::from(QSIZE)));
    mem.desc_table = Some(desc_table);
    mem.stable = kani::any();

    let result = walk_chain(&mem, desc_table, QSIZE, head);

    let n = mem.reads.get();
    let indices = *mem.indices.borrow();
    let mut looped = false;
    for a in 0..usize::from(QSIZE) {
        if a < n {
            assert!(indices[a] < u64::from(QSIZE));
            for b in 0..a {
                looped |= indices[a] == indices[b];
            }
        }
    }
    if mem.saw_indirect.get() {
        assert!(result.is_err(), "INDIRECT descriptor accepted");
    }
    if looped && mem.stable {
        assert!(result.is_err(), "looping chain accepted");
    }
    if let Ok(chain) = result {
        assert!(!chain.is_empty() && chain.len() <= usize::from(QSIZE));
        assert_eq!(chain.len(), n);
    }
}

/// (a) The blk parser never panics or overflows, for any chain, memory size and capacity.
#[kani::proof]
#[kani::unwind(5)]
fn blk_parse_no_panic() {
    for_any_chain(|chain| {
        let mem = KaniMem::new(kani::any());
        let _ = parse_blk_request(chain, &mem, kani::any());
    });
}

/// (b) Every data range and the status byte of an accepted request lie inside guest
/// memory.
#[kani::proof]
#[kani::unwind(5)]
fn blk_parse_ranges_in_memory() {
    for_any_chain(|chain| {
        let mem = KaniMem::new(kani::any());
        if let Ok(req) = parse_blk_request(chain, &mem, kani::any()) {
            for range in &req.data {
                assert!(in_memory(mem.size, range.addr, u64::from(range.len)));
            }
            assert!(in_memory(mem.size, req.status_addr, 1));
        }
    });
}

/// (c) For an accepted IN/OUT request, `sector * 512 + length <= capacity * 512` holds
/// in exact (u128) arithmetic, so no u64 overflow was possible; any IN/OUT request
/// reaching past the disk's end is rejected.
#[kani::proof]
#[kani::unwind(5)]
fn blk_parse_sector_bounds() {
    for_any_chain(|chain| {
        let Some(first) = chain.first() else {
            return;
        };
        let header: [u8; 16] = kani::any();
        let mut mem = KaniMem::new(kani::any());
        mem.pinned = Some((first.addr, header));
        let capacity: u64 = kani::any();

        let request_type = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let sector = u64::from_le_bytes([
            header[8], header[9], header[10], header[11], header[12], header[13], header[14],
            header[15],
        ]);
        let disk_end = capacity as u128 * SECTOR_SIZE as u128;
        let result = parse_blk_request(chain, &mem, capacity);
        if let Ok(req) = &result {
            if matches!(req.kind, BlkRequestType::In | BlkRequestType::Out) {
                let end = sector as u128 * SECTOR_SIZE as u128 + req.data_len() as u128;
                assert!(end <= disk_end, "request past the disk");
                assert_eq!(req.offset as u128, sector as u128 * SECTOR_SIZE as u128);
            }
        }
        if request_type == VIRTIO_BLK_T_IN || request_type == VIRTIO_BLK_T_OUT {
            // The data descriptors are the ones between the header and the status byte.
            let mut data_total: u128 = 0;
            for d in chain.iter().skip(1).take(chain.len().saturating_sub(2)) {
                data_total += d.len as u128;
            }
            if sector as u128 * SECTOR_SIZE as u128 + data_total > disk_end {
                assert!(result.is_err(), "out-of-range request accepted");
            }
        }
    });
}

/// (f) TX parsing never panics; an accepted chain has a total length in
/// `[12, 65_562]`, equal to the sum of its buffers, all inside guest memory.
#[kani::proof]
#[kani::unwind(5)]
fn net_tx_bounds() {
    for_any_chain(|chain| {
        let mem = KaniMem::new(kani::any());
        if let Ok(frame) = parse_net_tx(chain, &mem) {
            assert!(frame.len >= NET_HDR_SIZE && frame.len <= MAX_TX_LEN);
            let mut total: u128 = 0;
            for range in &frame.data {
                assert!(in_memory(mem.size, range.addr, u64::from(range.len)));
                total += range.len as u128;
            }
            assert_eq!(total, frame.len as u128);
        }
    });
}

/// (f) `zero_vnet_header` never panics on a buffer of any length (up to 64 here) and
/// leaves the header bytes that exist zero, and every later byte unchanged, so the
/// header handed to the TAP is all zeros.
#[kani::proof]
#[kani::unwind(65)]
fn net_tx_header_zeroed() {
    let original: [u8; 64] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= original.len());
    let mut buf = original;
    zero_vnet_header(&mut buf[..len]);
    for i in 0..len {
        if i < NET_HDR_SIZE {
            assert_eq!(buf[i], 0);
        } else {
            assert_eq!(buf[i], original[i]);
        }
    }
}
