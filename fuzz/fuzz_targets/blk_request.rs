//! Fuzz the guest-facing virtio-blk path: ring handling, the chain walker and the
//! request parser, over guest memory the fuzzer controls completely.
//!
//! Input: 8 bytes of disk capacity (sectors, little-endian), then a guest-memory image
//! (zero-padded or truncated to 64 KiB) with a fixed queue layout: queue size 16,
//! descriptor table at 0x0, available ring at 0x100, used ring at 0x200.
//! Beyond "never panics", every accepted request is checked against the same bounds
//! the Kani proofs state.

#![no_main]

use kvm_hello::devices::virtio::parse::{parse_blk_request, BlkRequestType, SECTOR_SIZE};
use kvm_hello::devices::virtio::queue::{walk_chain, GuestMem, Queue, VecMem};
use libfuzzer_sys::fuzz_target;

const MEM_SIZE: usize = 64 * 1024;
const QSIZE: u16 = 16;

fuzz_target!(|data: &[u8]| {
    let Some((capacity, image)) = data.split_first_chunk::<8>() else {
        return;
    };
    let capacity = u64::from_le_bytes(*capacity);
    let mut image = image.to_vec();
    image.resize(MEM_SIZE, 0);
    let mem = VecMem::from_bytes(image);

    let mut queue = Queue::default();
    queue.size = QSIZE;
    queue.ready = true;
    queue.desc_table = 0x0;
    queue.avail_ring = 0x100;
    queue.used_ring = 0x200;

    // At most QSIZE chains are pending (more is a broken ring), so this is bounded.
    for _ in 0..=QSIZE {
        let head = match queue.pop(&mem) {
            Ok(Some(head)) => head,
            Ok(None) | Err(_) => break,
        };
        assert!(head < QSIZE);
        let used_len = match walk_chain(&mem, queue.desc_table, queue.size, head) {
            Ok(chain) => {
                assert!(!chain.is_empty() && chain.len() <= usize::from(QSIZE));
                match parse_blk_request(&chain, &mem, capacity) {
                    Ok(req) => {
                        let size = mem.size() as u128;
                        for range in &req.data {
                            assert!(range.addr as u128 + range.len as u128 <= size);
                        }
                        assert!(req.status_addr as u128 + 1 <= size);
                        if req.kind != BlkRequestType::Flush {
                            let end = req.offset as u128 + req.data_len() as u128;
                            assert!(end <= capacity as u128 * SECTOR_SIZE as u128);
                        }
                        1
                    }
                    Err(_) => 0,
                }
            }
            Err(_) => 0,
        };
        if queue.add_used(&mem, head, used_len).is_err() {
            break;
        }
    }
});
