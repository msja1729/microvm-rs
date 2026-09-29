//! virtio-blk device: executes parsed requests against a host file.
//!
//! Parsing and all guest-controlled checks live in `parse.rs`; this file only moves
//! bytes between guest memory and the file, through one reused bounce buffer.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use log::{debug, warn};
use virtio_bindings::virtio_blk::{
    VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_RO, VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP,
};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::virtio_ids::VIRTIO_ID_BLOCK;

use super::parse::{
    blk_status_addr, parse_blk_request, BlkError, BlkRequest, BlkRequestType, SECTOR_SIZE,
};
use super::queue::{walk_chain, GuestMem, Queue, QueueError};
use super::VirtioDevice;

/// Size of the bounce buffer data is copied through, whatever the guest asks for.
const BOUNCE_SIZE: usize = 64 * 1024;

pub struct Block {
    file: File,
    capacity_sectors: u64,
    read_only: bool,
    /// Config space: `capacity` (u64, little-endian, in 512-byte sectors).
    config: [u8; 8],
    bounce: Vec<u8>,
}

impl Block {
    /// `size` is the file size in bytes; the caller checked it is a non-zero multiple
    /// of 512. With `read_only` the device advertises `VIRTIO_BLK_F_RO` and refuses
    /// every write, whatever the guest negotiated.
    pub fn new(file: File, size: u64, read_only: bool) -> Self {
        let capacity_sectors = size / SECTOR_SIZE;
        Self {
            file,
            capacity_sectors,
            read_only,
            config: capacity_sectors.to_le_bytes(),
            bounce: vec![0; BOUNCE_SIZE],
        }
    }

    /// Execute one parsed request. Returns the status byte and the number of bytes
    /// written into guest data buffers (not counting the status byte).
    fn execute<M: GuestMem + ?Sized>(&mut self, req: &BlkRequest, mem: &M) -> (u8, u64) {
        let mut written = 0;
        let result = match req.kind {
            BlkRequestType::Flush => self.file.sync_data(),
            BlkRequestType::Out if self.read_only => {
                debug!("virtio-blk: write to read-only disk rejected");
                return (VIRTIO_BLK_S_IOERR as u8, 0);
            }
            BlkRequestType::Out => self.copy_to_file(req, mem),
            BlkRequestType::In => self.copy_to_guest(req, mem, &mut written),
        };
        match result {
            Ok(()) => (VIRTIO_BLK_S_OK as u8, written),
            Err(e) => {
                warn!("virtio-blk: {:?} request failed: {e}", req.kind);
                (VIRTIO_BLK_S_IOERR as u8, written)
            }
        }
    }

    fn copy_to_file<M: GuestMem + ?Sized>(&mut self, req: &BlkRequest, mem: &M) -> io::Result<()> {
        let mut offset = req.offset;
        for range in &req.data {
            let mut done = 0u64;
            while done < u64::from(range.len) {
                let n = chunk_len(u64::from(range.len) - done);
                let chunk = &mut self.bounce[..n];
                mem.read(checked(range.addr.checked_add(done))?, chunk)
                    .map_err(|_| io::Error::other("guest memory read out of range"))?;
                self.file.write_all_at(chunk, offset)?;
                offset = checked(offset.checked_add(n as u64))?;
                done += n as u64;
            }
        }
        Ok(())
    }

    fn copy_to_guest<M: GuestMem + ?Sized>(
        &mut self,
        req: &BlkRequest,
        mem: &M,
        written: &mut u64,
    ) -> io::Result<()> {
        let mut offset = req.offset;
        for range in &req.data {
            let mut done = 0u64;
            while done < u64::from(range.len) {
                let n = chunk_len(u64::from(range.len) - done);
                let chunk = &mut self.bounce[..n];
                self.file.read_exact_at(chunk, offset)?;
                mem.write(checked(range.addr.checked_add(done))?, chunk)
                    .map_err(|_| io::Error::other("guest memory write out of range"))?;
                offset = checked(offset.checked_add(n as u64))?;
                done += n as u64;
                *written += n as u64;
            }
        }
        Ok(())
    }

    /// Handle the chain at `head` and return the used length to report for it.
    fn handle_chain<M: GuestMem + ?Sized>(&mut self, queue: &Queue, head: u16, mem: &M) -> u32 {
        let chain = match walk_chain(mem, queue.desc_table, queue.size, head) {
            Ok(chain) => chain,
            Err(e) => {
                debug!("virtio-blk: rejected descriptor chain {head}: {e:?}");
                return 0;
            }
        };
        let (status, written) = match parse_blk_request(&chain, mem, self.capacity_sectors) {
            Ok(req) => self.execute(&req, mem),
            Err(BlkError::Unsupported(t)) => {
                debug!("virtio-blk: unsupported request type {t}");
                (VIRTIO_BLK_S_UNSUPP as u8, 0)
            }
            Err(e) => {
                debug!("virtio-blk: rejected request: {e:?}");
                (VIRTIO_BLK_S_IOERR as u8, 0)
            }
        };
        let Some(status_addr) = blk_status_addr(&chain, mem) else {
            return 0;
        };
        if mem.write(status_addr, &[status]).is_err() {
            return 0;
        }
        // The guest can't build a request larger than u32::MAX bytes that fits in RAM
        // below 3 GiB, but saturate rather than truncate anyway.
        u32::try_from(written.saturating_add(1)).unwrap_or(u32::MAX)
    }
}

fn chunk_len(remaining: u64) -> usize {
    usize::try_from(remaining).map_or(BOUNCE_SIZE, |r| r.min(BOUNCE_SIZE))
}

/// Offsets were bounded by the parser; an overflow here would be a bug, reported as an
/// I/O error instead of a panic.
fn checked(value: Option<u64>) -> io::Result<u64> {
    value.ok_or_else(|| io::Error::other("offset overflow"))
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }

    fn features(&self) -> u64 {
        let mut features = (1 << VIRTIO_F_VERSION_1) | (1 << VIRTIO_BLK_F_FLUSH);
        if self.read_only {
            features |= 1 << VIRTIO_BLK_F_RO;
        }
        features
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn config(&self) -> &[u8] {
        &self.config
    }

    fn process_queue<M: GuestMem>(
        &mut self,
        _index: usize,
        queue: &mut Queue,
        mem: &M,
    ) -> Result<bool, QueueError> {
        let mut used = false;
        while let Some(head) = queue.pop(mem)? {
            let len = self.handle_chain(queue, head, mem);
            queue.add_used(mem, head, len)?;
            used = true;
        }
        Ok(used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::queue::tests::{publish, put, set_desc, test_queue, USED};
    use crate::devices::virtio::queue::VecMem;
    use std::io::{Read, Seek, Write};
    use virtio_bindings::virtio_blk::{
        VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
    };
    use virtio_bindings::virtio_ring::{VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};

    const HDR: u64 = 0x3000;
    const STATUS: u64 = 0x3100;
    const DATA: u64 = 0x4000;
    const DISK_SIZE: usize = 8 * 1024;
    const QSIZE: u16 = 8;

    fn disk(fill: u8) -> File {
        let mut file = tempfile::tempfile().expect("tempfile");
        file.write_all(&vec![fill; DISK_SIZE]).expect("fill disk");
        file
    }

    fn contents(file: &mut File) -> Vec<u8> {
        let mut v = Vec::new();
        file.rewind().expect("rewind");
        file.read_to_end(&mut v).expect("read disk");
        v
    }

    /// Build a 3-descriptor request (header, data, status) at descriptor 0 and publish it.
    fn request(mem: &VecMem, request_type: u32, sector: u64, data_len: u32, data_writable: bool) {
        put(mem, HDR, &request_type.to_le_bytes());
        put(mem, HDR + 8, &sector.to_le_bytes());
        put(mem, STATUS, &[0xaa]);
        set_desc(mem, 0, HDR, 16, VRING_DESC_F_NEXT, 1);
        let data_flags = VRING_DESC_F_NEXT | if data_writable { VRING_DESC_F_WRITE } else { 0 };
        set_desc(mem, 1, DATA, data_len, data_flags, 2);
        set_desc(mem, 2, STATUS, 1, VRING_DESC_F_WRITE, 0);
        publish(mem, QSIZE, &[0]);
    }

    fn status(mem: &VecMem) -> u8 {
        let mut b = [0u8];
        mem.read(STATUS, &mut b).expect("in range");
        b[0]
    }

    fn used_len(mem: &VecMem) -> u32 {
        let mut b = [0u8; 4];
        mem.read(USED + 8, &mut b).expect("in range");
        u32::from_le_bytes(b)
    }

    fn run(block: &mut Block, mem: &VecMem) {
        let mut q = test_queue(QSIZE);
        assert_eq!(block.process_queue(0, &mut q, mem), Ok(true));
    }

    #[test]
    fn read_and_write_round_trip() {
        let mut block = Block::new(disk(0x11), DISK_SIZE as u64, false);
        let mem = VecMem::new(0x2_0000);
        put(&mem, DATA, &[0x5a; 1024]);
        request(&mem, VIRTIO_BLK_T_OUT, 2, 1024, false);
        run(&mut block, &mem);
        assert_eq!(status(&mem), VIRTIO_BLK_S_OK as u8);
        assert_eq!(used_len(&mem), 1, "OUT writes only the status byte");
        let data = contents(&mut block.file);
        assert!(data[..1024].iter().all(|&b| b == 0x11));
        assert!(data[1024..2048].iter().all(|&b| b == 0x5a));
        assert!(data[2048..].iter().all(|&b| b == 0x11));

        let mem = VecMem::new(0x2_0000);
        request(&mem, VIRTIO_BLK_T_IN, 1, 1024, true);
        run(&mut block, &mem);
        assert_eq!(status(&mem), VIRTIO_BLK_S_OK as u8);
        assert_eq!(used_len(&mem), 1025);
        let mut buf = vec![0u8; 1024];
        mem.read(DATA, &mut buf).expect("in range");
        assert!(buf[..512].iter().all(|&b| b == 0x11));
        assert!(buf[512..].iter().all(|&b| b == 0x5a));
    }

    #[test]
    fn large_request_crosses_bounce_buffer_chunks() {
        const LEN: u32 = (BOUNCE_SIZE * 2 + 512) as u32;
        let file = tempfile::tempfile().expect("tempfile");
        file.set_len(u64::from(LEN)).expect("size");
        let mut block = Block::new(file, u64::from(LEN), false);
        let mem = VecMem::new(DATA as usize + LEN as usize);
        let pattern: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
        put(&mem, DATA, &pattern);
        request(&mem, VIRTIO_BLK_T_OUT, 0, LEN, false);
        run(&mut block, &mem);
        assert_eq!(status(&mem), VIRTIO_BLK_S_OK as u8);
        assert_eq!(contents(&mut block.file), pattern);
    }

    #[test]
    fn flush_succeeds() {
        let mut block = Block::new(disk(0), DISK_SIZE as u64, false);
        let mem = VecMem::new(0x2_0000);
        put(&mem, HDR, &VIRTIO_BLK_T_FLUSH.to_le_bytes());
        set_desc(&mem, 0, HDR, 16, VRING_DESC_F_NEXT, 1);
        set_desc(&mem, 1, STATUS, 1, VRING_DESC_F_WRITE, 0);
        publish(&mem, QSIZE, &[0]);
        run(&mut block, &mem);
        assert_eq!(status(&mem), VIRTIO_BLK_S_OK as u8);
    }

    /// AC-4.9: an OUT request on a read-only device fails with IOERR and leaves the
    /// backing file unchanged.
    #[test]
    fn out_on_read_only_device_is_ioerr_and_file_unchanged() {
        let mut block = Block::new(disk(0x33), DISK_SIZE as u64, true);
        assert_ne!(block.features() & (1 << VIRTIO_BLK_F_RO), 0);
        let before = contents(&mut block.file);
        let mem = VecMem::new(0x2_0000);
        put(&mem, DATA, &[0xee; 512]);
        request(&mem, VIRTIO_BLK_T_OUT, 0, 512, false);
        run(&mut block, &mem);
        assert_eq!(status(&mem), VIRTIO_BLK_S_IOERR as u8);
        assert_eq!(contents(&mut block.file), before);
    }

    /// AC-4.9: GET_ID and unknown request types complete with UNSUPP.
    #[test]
    fn get_id_and_unknown_types_are_unsupp() {
        for t in [VIRTIO_BLK_T_GET_ID, 99] {
            let mut block = Block::new(disk(0), DISK_SIZE as u64, false);
            let mem = VecMem::new(0x2_0000);
            request(&mem, t, 0, 20, true);
            run(&mut block, &mem);
            assert_eq!(status(&mem), VIRTIO_BLK_S_UNSUPP as u8, "type {t}");
            assert_eq!(used_len(&mem), 1);
        }
    }

    #[test]
    fn out_of_range_is_ioerr() {
        let mut block = Block::new(disk(0), DISK_SIZE as u64, false);
        let mem = VecMem::new(0x2_0000);
        request(&mem, VIRTIO_BLK_T_IN, 16, 512, true); // sector 16 of 16
        run(&mut block, &mem);
        assert_eq!(status(&mem), VIRTIO_BLK_S_IOERR as u8);
        assert_eq!(used_len(&mem), 1);
    }

    #[test]
    fn looping_chain_completes_with_zero_length() {
        let mut block = Block::new(disk(0), DISK_SIZE as u64, false);
        let mem = VecMem::new(0x2_0000);
        set_desc(&mem, 0, HDR, 16, VRING_DESC_F_NEXT, 1);
        set_desc(
            &mem,
            1,
            STATUS,
            1,
            VRING_DESC_F_NEXT | VRING_DESC_F_WRITE,
            0,
        );
        publish(&mem, QSIZE, &[0]);
        run(&mut block, &mem);
        assert_eq!(used_len(&mem), 0);
    }

    #[test]
    fn corrupt_ring_is_broken() {
        let mut block = Block::new(disk(0), DISK_SIZE as u64, false);
        let mem = VecMem::new(0x2_0000);
        put(
            &mem,
            crate::devices::virtio::queue::tests::AVAIL + 2,
            &100u16.to_le_bytes(),
        );
        let mut q = test_queue(QSIZE);
        assert_eq!(
            block.process_queue(0, &mut q, &mem),
            Err(QueueError::Broken)
        );
    }
}
