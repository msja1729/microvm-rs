//! Split virtqueue (virtio 1.x §2.7) over a small guest-memory trait.
//!
//! Everything here is guest-facing and pure: no KVM, no host files, no `vm-memory`.
//! Every value read from guest memory is untrusted. Each structure (a ring index, a
//! ring slot, a descriptor) is read exactly once into a local copy and checked before
//! it is used, so a guest rewriting memory concurrently cannot slip past a check.
//!
//! Not supported (and never offered to the guest): indirect descriptors, event-index
//! notification suppression, packed virtqueues.

use std::sync::atomic::{fence, Ordering};

use virtio_bindings::virtio_ring::{VRING_DESC_F_INDIRECT, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};

/// Largest queue size the device offers (`QueueNumMax`).
pub const MAX_QUEUE_SIZE: u16 = 256;

/// Size in bytes of one descriptor-table entry.
const DESC_SIZE: u64 = 16;
/// Offset of `idx` in the available and used rings (after the 2-byte `flags`).
const RING_IDX_OFFSET: u64 = 2;
/// Offset of the first ring slot in the available and used rings.
const RING_SLOTS_OFFSET: u64 = 4;
/// Size of one available-ring slot (a `u16` descriptor index).
const AVAIL_SLOT_SIZE: u64 = 2;
/// Size of one used-ring slot (`u32` id + `u32` len).
const USED_SLOT_SIZE: u64 = 8;

/// A guest-memory access fell outside guest RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemError;

/// Guest RAM as one contiguous range `[0, size())`.
pub trait GuestMem {
    /// RAM size in bytes.
    fn size(&self) -> u64;
    /// Fill `buf` from guest-physical `addr`. Fails if any byte is outside RAM.
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), MemError>;
    /// Copy `buf` to guest-physical `addr`. Fails if any byte is outside RAM.
    fn write(&self, addr: u64, buf: &[u8]) -> Result<(), MemError>;
}

/// `true` if `[addr, addr + len)` lies inside `[0, size)`, computed without overflow.
pub fn range_in_memory(size: u64, addr: u64, len: u64) -> bool {
    addr.checked_add(len).is_some_and(|end| end <= size)
}

fn read_u16<M: GuestMem + ?Sized>(mem: &M, addr: u64) -> Result<u16, MemError> {
    let mut b = [0u8; 2];
    mem.read(addr, &mut b)?;
    Ok(u16::from_le_bytes(b))
}

/// Guest memory backed by a byte vector: used by unit tests and the fuzzer.
/// Single-threaded; writes go through a `RefCell` so `GuestMem` can take `&self`.
#[derive(Debug, Clone, Default)]
pub struct VecMem(std::cell::RefCell<Vec<u8>>);

impl VecMem {
    /// `size` bytes of zeroed memory.
    pub fn new(size: usize) -> Self {
        Self::from_bytes(vec![0; size])
    }

    /// Memory whose contents are `bytes`.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(std::cell::RefCell::new(bytes))
    }

    fn range(total: usize, addr: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(addr).ok()?;
        let end = start.checked_add(len)?;
        (end <= total).then_some(start..end)
    }
}

impl GuestMem for VecMem {
    fn size(&self) -> u64 {
        self.0.borrow().len() as u64
    }

    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), MemError> {
        let mem = self.0.borrow();
        let range = Self::range(mem.len(), addr, buf.len()).ok_or(MemError)?;
        buf.copy_from_slice(mem.get(range).ok_or(MemError)?);
        Ok(())
    }

    fn write(&self, addr: u64, buf: &[u8]) -> Result<(), MemError> {
        let mut mem = self.0.borrow_mut();
        let range = Self::range(mem.len(), addr, buf.len()).ok_or(MemError)?;
        mem.get_mut(range).ok_or(MemError)?.copy_from_slice(buf);
        Ok(())
    }
}

/// The ring itself is corrupt; the queue cannot safely continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// `avail.idx` moved by more than the queue size, a head index is out of range, a
    /// ring lies outside RAM, or the queue has no valid size.
    Broken,
}

/// One descriptor chain was malformed; only that request fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// A descriptor index (head or `next`) is `>= queue size`.
    BadIndex,
    /// A descriptor has the `INDIRECT` flag, which is never negotiated.
    Indirect,
    /// The chain has more descriptors than the queue size, i.e. it loops.
    TooLong,
    /// A descriptor-table entry lies outside RAM.
    Memory,
}

/// One descriptor of a chain, as copied out of guest memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainDesc {
    pub addr: u64,
    pub len: u32,
    /// The device may write to this buffer (`VRING_DESC_F_WRITE`).
    pub writable: bool,
}

/// Follow the descriptor chain starting at `head` in the table at `desc_table`.
///
/// Runs at most `size` iterations: an index `>= size`, an `INDIRECT` descriptor, or a
/// chain longer than `size` descriptors (the only way a chain can revisit an index)
/// is rejected. Returns between 1 and `size` descriptors.
pub fn walk_chain<M: GuestMem + ?Sized>(
    mem: &M,
    desc_table: u64,
    size: u16,
    head: u16,
) -> Result<Vec<ChainDesc>, ChainError> {
    let mut chain = Vec::new();
    let mut index = head;
    loop {
        if index >= size {
            return Err(ChainError::BadIndex);
        }
        if chain.len() >= usize::from(size) {
            return Err(ChainError::TooLong);
        }
        let addr = u64::from(index)
            .checked_mul(DESC_SIZE)
            .and_then(|off| desc_table.checked_add(off))
            .ok_or(ChainError::Memory)?;
        // Read the whole 16-byte descriptor once, then decode the local copy.
        let mut raw = [0u8; DESC_SIZE as usize];
        mem.read(addr, &mut raw).map_err(|_| ChainError::Memory)?;
        let [a0, a1, a2, a3, a4, a5, a6, a7, l0, l1, l2, l3, f0, f1, n0, n1] = raw;
        let flags = u32::from(u16::from_le_bytes([f0, f1]));
        if flags & VRING_DESC_F_INDIRECT != 0 {
            return Err(ChainError::Indirect);
        }
        chain.push(ChainDesc {
            addr: u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
            len: u32::from_le_bytes([l0, l1, l2, l3]),
            writable: flags & VRING_DESC_F_WRITE != 0,
        });
        if flags & VRING_DESC_F_NEXT == 0 {
            return Ok(chain);
        }
        index = u16::from_le_bytes([n0, n1]);
    }
}

/// Driver-visible configuration and device-side position of one split virtqueue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queue {
    /// Number of entries chosen by the driver (`QueueNum`).
    pub size: u16,
    /// `QueueReady`.
    pub ready: bool,
    /// Guest-physical addresses of the descriptor table, available ring and used ring.
    pub desc_table: u64,
    pub avail_ring: u64,
    pub used_ring: u64,
    /// Next available-ring slot the device will consume (free-running, wraps at 2^16).
    next_avail: u16,
    /// Next used-ring slot the device will fill (free-running, wraps at 2^16).
    next_used: u16,
}

impl Queue {
    /// `true` if the driver's configuration is one the device can safely use: the size
    /// is a power of two in `1..=MAX_QUEUE_SIZE`, and each ring is aligned as the virtio
    /// spec requires and lies entirely inside guest RAM.
    pub fn is_valid(&self, mem_size: u64) -> bool {
        let size = u64::from(self.size);
        if !self.size.is_power_of_two() || self.size > MAX_QUEUE_SIZE {
            return false;
        }
        // Ring sizes include the trailing `used_event`/`avail_event` field, as the
        // virtio spec defines them, even though event index is never negotiated.
        let desc_bytes = DESC_SIZE * size;
        let avail_bytes = RING_SLOTS_OFFSET + AVAIL_SLOT_SIZE * size + 2;
        let used_bytes = RING_SLOTS_OFFSET + USED_SLOT_SIZE * size + 2;
        self.desc_table.is_multiple_of(16)
            && self.avail_ring.is_multiple_of(2)
            && self.used_ring.is_multiple_of(4)
            && range_in_memory(mem_size, self.desc_table, desc_bytes)
            && range_in_memory(mem_size, self.avail_ring, avail_bytes)
            && range_in_memory(mem_size, self.used_ring, used_bytes)
    }

    /// Take the next available descriptor-chain head, if the driver published one.
    pub fn pop<M: GuestMem + ?Sized>(&mut self, mem: &M) -> Result<Option<u16>, QueueError> {
        if self.size == 0 {
            return Err(QueueError::Broken);
        }
        let idx_addr = self
            .avail_ring
            .checked_add(RING_IDX_OFFSET)
            .ok_or(QueueError::Broken)?;
        let avail_idx = read_u16(mem, idx_addr).map_err(|_| QueueError::Broken)?;
        // Pairs with the driver's write barrier before it publishes `avail.idx`: ring
        // slots read below are at least as new as the index we just read.
        fence(Ordering::Acquire);

        let pending = avail_idx.wrapping_sub(self.next_avail);
        if pending == 0 {
            return Ok(None);
        }
        if pending > self.size {
            return Err(QueueError::Broken);
        }
        let slot = u64::from(self.next_avail % self.size);
        let slot_addr = slot
            .checked_mul(AVAIL_SLOT_SIZE)
            .and_then(|off| off.checked_add(RING_SLOTS_OFFSET))
            .and_then(|off| self.avail_ring.checked_add(off))
            .ok_or(QueueError::Broken)?;
        let head = read_u16(mem, slot_addr).map_err(|_| QueueError::Broken)?;
        if head >= self.size {
            return Err(QueueError::Broken);
        }
        self.next_avail = self.next_avail.wrapping_add(1);
        Ok(Some(head))
    }

    /// Return chain `head` to the driver, reporting `len` bytes written by the device.
    pub fn add_used<M: GuestMem + ?Sized>(
        &mut self,
        mem: &M,
        head: u16,
        len: u32,
    ) -> Result<(), QueueError> {
        if self.size == 0 {
            return Err(QueueError::Broken);
        }
        let slot = u64::from(self.next_used % self.size);
        let slot_addr = slot
            .checked_mul(USED_SLOT_SIZE)
            .and_then(|off| off.checked_add(RING_SLOTS_OFFSET))
            .and_then(|off| self.used_ring.checked_add(off))
            .ok_or(QueueError::Broken)?;
        let mut elem = [0u8; USED_SLOT_SIZE as usize];
        let (id, used_len) = elem.split_at_mut(4);
        id.copy_from_slice(&u32::from(head).to_le_bytes());
        used_len.copy_from_slice(&len.to_le_bytes());
        mem.write(slot_addr, &elem)
            .map_err(|_| QueueError::Broken)?;

        // The used element must be visible before the index that publishes it.
        fence(Ordering::Release);
        self.next_used = self.next_used.wrapping_add(1);
        let idx_addr = self
            .used_ring
            .checked_add(RING_IDX_OFFSET)
            .ok_or(QueueError::Broken)?;
        mem.write(idx_addr, &self.next_used.to_le_bytes())
            .map_err(|_| QueueError::Broken)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const DESC: u64 = 0x0;
    pub const AVAIL: u64 = 0x1000;
    pub const USED: u64 = 0x2000;

    /// A queue of `size` entries with the rings at the fixed test addresses.
    pub fn test_queue(size: u16) -> Queue {
        Queue {
            size,
            ready: true,
            desc_table: DESC,
            avail_ring: AVAIL,
            used_ring: USED,
            ..Default::default()
        }
    }

    pub fn put(mem: &VecMem, addr: u64, bytes: &[u8]) {
        mem.write(addr, bytes).expect("test write in range");
    }

    pub fn set_desc(mem: &VecMem, index: u16, addr: u64, len: u32, flags: u32, next: u16) {
        let base = DESC + 16 * u64::from(index);
        put(mem, base, &addr.to_le_bytes());
        put(mem, base + 8, &len.to_le_bytes());
        put(mem, base + 12, &(flags as u16).to_le_bytes());
        put(mem, base + 14, &next.to_le_bytes());
    }

    /// Publish `heads` in the available ring, starting at slot 0.
    pub fn publish(mem: &VecMem, size: u16, heads: &[u16]) {
        for (i, head) in heads.iter().enumerate() {
            put(
                mem,
                AVAIL + 4 + 2 * (i as u64 % u64::from(size)),
                &head.to_le_bytes(),
            );
        }
        put(mem, AVAIL + 2, &(heads.len() as u16).to_le_bytes());
    }

    fn read_u32(mem: &VecMem, addr: u64) -> u32 {
        let mut b = [0u8; 4];
        mem.read(addr, &mut b).expect("in range");
        u32::from_le_bytes(b)
    }

    #[test]
    fn walk_single_and_multi_descriptor_chains() {
        let mem = VecMem::new(0x3000);
        set_desc(&mem, 0, 0x100, 16, VRING_DESC_F_NEXT, 2);
        set_desc(
            &mem,
            2,
            0x200,
            512,
            VRING_DESC_F_NEXT | VRING_DESC_F_WRITE,
            1,
        );
        set_desc(&mem, 1, 0x400, 1, VRING_DESC_F_WRITE, 0);
        let chain = walk_chain(&mem, DESC, 4, 0).expect("valid chain");
        assert_eq!(
            chain,
            vec![
                ChainDesc {
                    addr: 0x100,
                    len: 16,
                    writable: false
                },
                ChainDesc {
                    addr: 0x200,
                    len: 512,
                    writable: true
                },
                ChainDesc {
                    addr: 0x400,
                    len: 1,
                    writable: true
                },
            ]
        );
        assert_eq!(walk_chain(&mem, DESC, 4, 1).expect("valid").len(), 1);
    }

    #[test]
    fn walk_rejects_loop() {
        let mem = VecMem::new(0x3000);
        set_desc(&mem, 0, 0x100, 16, VRING_DESC_F_NEXT, 1);
        set_desc(&mem, 1, 0x200, 16, VRING_DESC_F_NEXT, 0);
        assert_eq!(walk_chain(&mem, DESC, 4, 0), Err(ChainError::TooLong));
        // A descriptor pointing at itself.
        set_desc(&mem, 3, 0x100, 16, VRING_DESC_F_NEXT, 3);
        assert_eq!(walk_chain(&mem, DESC, 4, 3), Err(ChainError::TooLong));
    }

    #[test]
    fn walk_rejects_indirect() {
        let mem = VecMem::new(0x3000);
        set_desc(&mem, 0, 0x100, 16, VRING_DESC_F_NEXT, 1);
        set_desc(&mem, 1, 0x200, 32, VRING_DESC_F_INDIRECT, 0);
        assert_eq!(walk_chain(&mem, DESC, 4, 0), Err(ChainError::Indirect));
    }

    #[test]
    fn walk_rejects_index_out_of_range() {
        let mem = VecMem::new(0x3000);
        assert_eq!(walk_chain(&mem, DESC, 4, 4), Err(ChainError::BadIndex));
        set_desc(&mem, 0, 0x100, 16, VRING_DESC_F_NEXT, 9);
        assert_eq!(walk_chain(&mem, DESC, 4, 0), Err(ChainError::BadIndex));
    }

    #[test]
    fn walk_rejects_table_outside_memory() {
        let mem = VecMem::new(0x20);
        assert_eq!(walk_chain(&mem, DESC, 4, 2), Err(ChainError::Memory));
        assert_eq!(
            walk_chain(&mem, u64::MAX - 8, 4, 0),
            Err(ChainError::Memory)
        );
    }

    #[test]
    fn pop_returns_heads_in_order_then_none() {
        let mem = VecMem::new(0x3000);
        let mut q = test_queue(4);
        publish(&mem, 4, &[2, 0]);
        assert_eq!(q.pop(&mem), Ok(Some(2)));
        assert_eq!(q.pop(&mem), Ok(Some(0)));
        assert_eq!(q.pop(&mem), Ok(None));
    }

    #[test]
    fn pop_rejects_corrupt_avail_idx() {
        let mem = VecMem::new(0x3000);
        let mut q = test_queue(4);
        // avail.idx jumps 5 ahead with a queue of 4 entries.
        put(&mem, AVAIL + 2, &5u16.to_le_bytes());
        assert_eq!(q.pop(&mem), Err(QueueError::Broken));
        // A wrapped-around index ("-1") is also more than `size` ahead.
        put(&mem, AVAIL + 2, &u16::MAX.to_le_bytes());
        assert_eq!(q.pop(&mem), Err(QueueError::Broken));
    }

    #[test]
    fn pop_rejects_head_out_of_range() {
        let mem = VecMem::new(0x3000);
        let mut q = test_queue(4);
        publish(&mem, 4, &[4]);
        assert_eq!(q.pop(&mem), Err(QueueError::Broken));
    }

    #[test]
    fn pop_with_zero_size_is_broken() {
        let mem = VecMem::new(0x3000);
        let mut q = test_queue(0);
        assert_eq!(q.pop(&mem), Err(QueueError::Broken));
        assert_eq!(q.add_used(&mem, 0, 0), Err(QueueError::Broken));
    }

    #[test]
    fn add_used_writes_element_then_index() {
        let mem = VecMem::new(0x3000);
        let mut q = test_queue(2);
        q.add_used(&mem, 1, 513).expect("in range");
        q.add_used(&mem, 0, 7).expect("in range");
        q.add_used(&mem, 1, 9).expect("in range");
        // Slot 0 was reused by the third element (next_used 2 % size 2 = 0).
        assert_eq!(read_u32(&mem, USED + 4), 1);
        assert_eq!(read_u32(&mem, USED + 8), 9);
        assert_eq!(read_u32(&mem, USED + 12), 0);
        assert_eq!(read_u32(&mem, USED + 16), 7);
        let mut idx = [0u8; 2];
        mem.read(USED + 2, &mut idx).expect("in range");
        assert_eq!(u16::from_le_bytes(idx), 3);
    }

    #[test]
    fn queue_validation() {
        let mut q = test_queue(256);
        assert!(q.is_valid(0x4000));
        q.size = 3;
        assert!(!q.is_valid(0x4000), "not a power of two");
        q.size = 512;
        assert!(!q.is_valid(0x1_0000), "larger than QueueNumMax");
        q.size = 0;
        assert!(!q.is_valid(0x4000), "zero size");
        let mut q = test_queue(256);
        q.used_ring = 0x2002;
        assert!(!q.is_valid(0x4000), "misaligned used ring");
        let mut q = test_queue(256);
        assert!(!q.is_valid(0x2000), "used ring past the end of RAM");
        q.desc_table = !0xf;
        assert!(!q.is_valid(u64::MAX), "descriptor table wraps around");
    }

    #[test]
    fn range_check_does_not_overflow() {
        assert!(range_in_memory(10, 0, 10));
        assert!(!range_in_memory(10, 1, 10));
        assert!(!range_in_memory(u64::MAX, u64::MAX, 1));
        assert!(range_in_memory(u64::MAX, u64::MAX, 0));
    }
}
