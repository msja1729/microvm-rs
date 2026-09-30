//! virtio-net device backed by a host TAP interface.
//!
//! Queue 0 is RX (host → guest), queue 1 is TX (guest → host). Each TAP `read` or
//! `write` is exactly one frame, preceded by the 12-byte virtio-net header. Parsing and
//! the guest-controlled checks are in `parse.rs`.

use std::io::{self, Read, Write};

use log::{debug, warn};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::virtio_ids::VIRTIO_ID_NET;
use virtio_bindings::virtio_net::VIRTIO_NET_F_MAC;

use super::parse::{parse_net_rx, parse_net_tx, zero_vnet_header, MAX_TX_LEN, NET_HDR_SIZE};
use super::queue::{walk_chain, GuestMem, Queue, QueueError};
use super::VirtioDevice;

pub const RX_QUEUE: usize = 0;
pub const TX_QUEUE: usize = 1;
/// Guest MAC address used when `--guest-mac` is not given.
pub const DEFAULT_MAC: [u8; 6] = [0x06, 0x00, 0xac, 0x10, 0x00, 0x02];
/// Offset of `num_buffers` in the virtio 1.x net header.
const NUM_BUFFERS_OFFSET: usize = 10;

/// Parse `aa:bb:cc:dd:ee:ff` (exactly six 2-digit hex octets). Multicast addresses
/// (bit 0 of the first octet set) are rejected: a NIC must have a unicast address.
pub fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let bad =
        || format!("invalid MAC address '{s}': expected six hex octets like 06:00:ac:10:00:02");
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for octet in mac.iter_mut() {
        let part = parts.next().ok_or_else(bad)?;
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        *octet = u8::from_str_radix(part, 16).map_err(|_| bad())?;
    }
    if parts.next().is_some() {
        return Err(bad());
    }
    if mac[0] & 1 != 0 {
        return Err(format!(
            "invalid MAC address '{s}': it is a multicast address"
        ));
    }
    Ok(mac)
}

pub struct Net<T: Read + Write + Send> {
    tap: T,
    mac: [u8; 6],
    rx_buf: Vec<u8>,
    tx_buf: Vec<u8>,
    /// The last RX pass stopped because the guest had no buffers while the TAP may
    /// still hold frames. The event loop then stops watching the TAP (epoll is
    /// level-triggered and would spin) until the guest adds RX buffers.
    rx_starved: bool,
}

impl<T: Read + Write + Send> Net<T> {
    pub fn new(tap: T, mac: [u8; 6]) -> Self {
        Self {
            tap,
            mac,
            rx_buf: vec![0; MAX_TX_LEN],
            tx_buf: vec![0; MAX_TX_LEN],
            rx_starved: false,
        }
    }

    pub fn rx_starved(&self) -> bool {
        self.rx_starved
    }

    /// Move frames from the TAP into guest RX buffers until either runs out.
    fn process_rx<M: GuestMem>(&mut self, queue: &mut Queue, mem: &M) -> Result<bool, QueueError> {
        let mut used = false;
        loop {
            // Only read a frame when there is a chain to put it in, so frames wait in
            // the TAP's queue (not in the VMM) while the guest is out of buffers.
            if !queue.has_available(mem)? {
                self.rx_starved = true;
                return Ok(used);
            }
            let len = match self.tap.read(&mut self.rx_buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("virtio-net: TAP read failed: {e}");
                    break;
                }
            };
            let Some(head) = queue.pop(mem)? else {
                // `has_available` said yes; the guest must have moved `avail.idx` back.
                debug!("virtio-net: RX buffer vanished; frame dropped");
                self.rx_starved = true;
                return Ok(used);
            };
            let used_len = self.deliver(queue, head, len, mem);
            queue.add_used(mem, head, used_len)?;
            used = true;
        }
        self.rx_starved = false;
        Ok(used)
    }

    /// Scatter the `len`-byte frame in `rx_buf` into chain `head`. Returns the used
    /// length: `len`, or 0 if the chain is invalid or too small (the frame is dropped).
    fn deliver<M: GuestMem>(&mut self, queue: &Queue, head: u16, len: usize, mem: &M) -> u32 {
        let Some(frame) = self
            .rx_buf
            .get_mut(..len)
            .filter(|f| f.len() >= NET_HDR_SIZE)
        else {
            debug!("virtio-net: TAP frame shorter than the header; dropped");
            return 0;
        };
        // Without VIRTIO_NET_F_MRG_RXBUF every frame uses exactly one buffer chain.
        frame[NUM_BUFFERS_OFFSET..NET_HDR_SIZE].copy_from_slice(&1u16.to_le_bytes());
        let chain = match walk_chain(mem, queue.desc_table, queue.size, head) {
            Ok(chain) => chain,
            Err(e) => {
                debug!("virtio-net: rejected RX chain {head}: {e:?}");
                return 0;
            }
        };
        let ranges = match parse_net_rx(&chain, mem) {
            Ok((ranges, capacity)) if capacity >= len as u64 => ranges,
            Ok(_) => {
                debug!("virtio-net: {len}-byte frame does not fit RX chain {head}; dropped");
                return 0;
            }
            Err(e) => {
                debug!("virtio-net: rejected RX chain {head}: {e:?}");
                return 0;
            }
        };
        let mut rest: &[u8] = frame;
        for range in ranges {
            if rest.is_empty() {
                break;
            }
            let take = rest.len().min(range.len as usize);
            let (chunk, tail) = rest.split_at(take);
            if mem.write(range.addr, chunk).is_err() {
                return 0;
            }
            rest = tail;
        }
        // len <= MAX_TX_LEN, far below u32::MAX.
        u32::try_from(len).unwrap_or(0)
    }

    /// Send every available TX chain to the TAP. Every chain is completed with used
    /// length 0 (the device writes nothing into TX buffers).
    fn process_tx<M: GuestMem>(&mut self, queue: &mut Queue, mem: &M) -> Result<bool, QueueError> {
        let mut used = false;
        while let Some(head) = queue.pop(mem)? {
            self.transmit(queue, head, mem);
            queue.add_used(mem, head, 0)?;
            used = true;
        }
        Ok(used)
    }

    fn transmit<M: GuestMem>(&mut self, queue: &Queue, head: u16, mem: &M) {
        let chain = match walk_chain(mem, queue.desc_table, queue.size, head) {
            Ok(chain) => chain,
            Err(e) => {
                debug!("virtio-net: rejected TX chain {head}: {e:?}");
                return;
            }
        };
        let frame = match parse_net_tx(&chain, mem) {
            Ok(frame) => frame,
            Err(e) => {
                debug!("virtio-net: rejected TX chain {head}: {e:?}");
                return;
            }
        };
        // Gather into the fixed buffer; parse_net_tx bounded the total by its size.
        let mut pos = 0usize;
        for range in &frame.data {
            let end = pos.saturating_add(range.len as usize);
            let Some(dst) = self.tx_buf.get_mut(pos..end) else {
                return;
            };
            if mem.read(range.addr, dst).is_err() {
                return;
            }
            pos = end;
        }
        let Some(out) = self.tx_buf.get_mut(..frame.len) else {
            return;
        };
        zero_vnet_header(out);
        match self.tap.write(out) {
            Ok(_) => {}
            // The TAP's queue is full: drop the frame, as Ethernet may.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                debug!("virtio-net: TAP full; TX frame dropped")
            }
            Err(e) => warn!("virtio-net: TAP write failed: {e}"),
        }
    }
}

impl<T: Read + Write + Send> VirtioDevice for Net<T> {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_NET
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1) | (1 << VIRTIO_NET_F_MAC)
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn config(&self) -> &[u8] {
        &self.mac
    }

    fn process_queue<M: GuestMem>(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &M,
    ) -> Result<bool, QueueError> {
        match index {
            RX_QUEUE => self.process_rx(queue, mem),
            TX_QUEUE => self.process_tx(queue, mem),
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::queue::tests::{publish, put, set_desc, test_queue, USED};
    use crate::devices::virtio::queue::VecMem;
    use std::collections::VecDeque;
    use virtio_bindings::virtio_ring::{VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};

    /// A fake TAP: `rx` frames are returned by `read`, `write`s land in `tx`.
    #[derive(Default)]
    struct FakeTap {
        rx: VecDeque<Vec<u8>>,
        tx: Vec<Vec<u8>>,
        full: bool,
    }

    impl Read for FakeTap {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let frame = self.rx.pop_front().ok_or(io::ErrorKind::WouldBlock)?;
            buf[..frame.len()].copy_from_slice(&frame);
            Ok(frame.len())
        }
    }

    impl Write for FakeTap {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.full {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.tx.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const QSIZE: u16 = 8;
    const BUF: u64 = 0x4000;

    fn used(mem: &VecMem, slot: u64) -> (u32, u32) {
        let mut b = [0u8; 8];
        mem.read(USED + 4 + 8 * slot, &mut b).expect("in range");
        (
            u32::from_le_bytes(b[..4].try_into().expect("4 bytes")),
            u32::from_le_bytes(b[4..].try_into().expect("4 bytes")),
        )
    }

    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8 + 1).collect()
    }

    #[test]
    fn mac_parser() {
        assert_eq!(parse_mac("06:00:ac:10:00:02"), Ok(DEFAULT_MAC));
        assert_eq!(
            parse_mac("AA:bb:cc:dd:ee:0F"),
            Ok([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x0f])
        );
        for bad in [
            "",
            "06:00:ac:10:00",
            "06:00:ac:10:00:02:03",
            "6:00:ac:10:00:02",
            "06-00-ac-10-00-02",
            "06:00:ac:10:00:0g",
            "06:00:ac:10:00:+2",
            "06:00:ac:10:00:002",
        ] {
            assert!(parse_mac(bad).is_err(), "{bad}");
        }
        let err = parse_mac("01:00:5e:00:00:01").expect_err("multicast");
        assert!(err.contains("multicast"));
        assert!(
            parse_mac("ff:ff:ff:ff:ff:ff").is_err(),
            "broadcast is multicast"
        );
    }

    #[test]
    fn rx_scatters_frame_across_descriptors() {
        let f = frame(100);
        let tap = FakeTap {
            rx: VecDeque::from([f.clone()]),
            ..Default::default()
        };
        let mut net = Net::new(tap, DEFAULT_MAC);
        let mem = VecMem::new(0x1_0000);
        set_desc(&mem, 0, BUF, 12, VRING_DESC_F_NEXT | VRING_DESC_F_WRITE, 1);
        set_desc(
            &mem,
            1,
            BUF + 0x100,
            50,
            VRING_DESC_F_NEXT | VRING_DESC_F_WRITE,
            2,
        );
        set_desc(&mem, 2, BUF + 0x200, 1518, VRING_DESC_F_WRITE, 0);
        publish(&mem, QSIZE, &[0]);
        let mut q = test_queue(QSIZE);
        assert_eq!(net.process_queue(RX_QUEUE, &mut q, &mem), Ok(true));
        assert_eq!(used(&mem, 0), (0, 100));
        let mut got = vec![0u8; 100];
        mem.read(BUF, &mut got[..12]).expect("in range");
        mem.read(BUF + 0x100, &mut got[12..62]).expect("in range");
        mem.read(BUF + 0x200, &mut got[62..]).expect("in range");
        let mut want = f;
        want[10..12].copy_from_slice(&1u16.to_le_bytes()); // num_buffers = 1
        assert_eq!(got, want);
        assert!(net.rx_starved(), "its only buffer was used");
    }

    #[test]
    fn rx_with_spare_buffers_drains_tap_and_is_not_starved() {
        let tap = FakeTap {
            rx: VecDeque::from([frame(60)]),
            ..Default::default()
        };
        let mut net = Net::new(tap, DEFAULT_MAC);
        let mem = VecMem::new(0x1_0000);
        set_desc(&mem, 0, BUF, 1530, VRING_DESC_F_WRITE, 0);
        set_desc(&mem, 1, BUF + 0x1000, 1530, VRING_DESC_F_WRITE, 0);
        publish(&mem, QSIZE, &[0, 1]);
        let mut q = test_queue(QSIZE);
        assert_eq!(net.process_queue(RX_QUEUE, &mut q, &mem), Ok(true));
        assert_eq!(used(&mem, 0), (0, 60));
        assert!(!net.rx_starved(), "TAP empty with a buffer to spare");
    }

    #[test]
    fn rx_frame_larger_than_chain_is_dropped() {
        let tap = FakeTap {
            rx: VecDeque::from([frame(200)]),
            ..Default::default()
        };
        let mut net = Net::new(tap, DEFAULT_MAC);
        let mem = VecMem::new(0x1_0000);
        set_desc(&mem, 0, BUF, 100, VRING_DESC_F_WRITE, 0);
        publish(&mem, QSIZE, &[0]);
        let mut q = test_queue(QSIZE);
        assert_eq!(net.process_queue(RX_QUEUE, &mut q, &mem), Ok(true));
        assert_eq!(used(&mem, 0), (0, 0));
        let mut untouched = [0xffu8; 100];
        mem.read(BUF, &mut untouched).expect("in range");
        assert!(untouched.iter().all(|&b| b == 0), "nothing written");
    }

    #[test]
    fn rx_without_buffers_is_starved_and_keeps_frames_in_tap() {
        let tap = FakeTap {
            rx: VecDeque::from([frame(60), frame(70)]),
            ..Default::default()
        };
        let mut net = Net::new(tap, DEFAULT_MAC);
        let mem = VecMem::new(0x1_0000);
        set_desc(&mem, 0, BUF, 1530, VRING_DESC_F_WRITE, 0);
        publish(&mem, QSIZE, &[0]);
        let mut q = test_queue(QSIZE);
        assert_eq!(net.process_queue(RX_QUEUE, &mut q, &mem), Ok(true));
        assert!(net.rx_starved());
        assert_eq!(net.tap.rx.len(), 1, "second frame still queued in the TAP");
    }

    #[test]
    fn rx_rejects_readable_descriptor() {
        let tap = FakeTap {
            rx: VecDeque::from([frame(60)]),
            ..Default::default()
        };
        let mut net = Net::new(tap, DEFAULT_MAC);
        let mem = VecMem::new(0x1_0000);
        set_desc(&mem, 0, BUF, 1530, 0, 0);
        publish(&mem, QSIZE, &[0]);
        let mut q = test_queue(QSIZE);
        assert_eq!(net.process_queue(RX_QUEUE, &mut q, &mem), Ok(true));
        assert_eq!(used(&mem, 0), (0, 0));
    }

    #[test]
    fn tx_gathers_zeroes_header_and_sends_one_frame() {
        let mut net = Net::new(FakeTap::default(), DEFAULT_MAC);
        let mem = VecMem::new(0x1_0000);
        put(&mem, BUF, &[0xee; 12]); // guest-supplied header: must not reach the TAP
        put(&mem, BUF + 0x100, &[1, 2, 3, 4]);
        set_desc(&mem, 0, BUF, 12, VRING_DESC_F_NEXT, 1);
        set_desc(&mem, 1, BUF + 0x100, 4, 0, 0);
        // A second chain with a device-writable descriptor: rejected, not sent.
        set_desc(&mem, 2, BUF, 64, VRING_DESC_F_WRITE, 0);
        publish(&mem, QSIZE, &[0, 2]);
        let mut q = test_queue(QSIZE);
        assert_eq!(net.process_queue(TX_QUEUE, &mut q, &mem), Ok(true));
        let mut want = vec![0u8; 12];
        want.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(net.tap.tx, vec![want]);
        assert_eq!(used(&mem, 0), (0, 0));
        assert_eq!(used(&mem, 1), (2, 0));
    }

    #[test]
    fn tx_drops_frame_when_tap_is_full() {
        let tap = FakeTap {
            full: true,
            ..Default::default()
        };
        let mut net = Net::new(tap, DEFAULT_MAC);
        let mem = VecMem::new(0x1_0000);
        set_desc(&mem, 0, BUF, 60, 0, 0);
        publish(&mem, QSIZE, &[0]);
        let mut q = test_queue(QSIZE);
        assert_eq!(net.process_queue(TX_QUEUE, &mut q, &mem), Ok(true));
        assert_eq!(used(&mem, 0), (0, 0), "completed even though dropped");
        assert!(net.tap.tx.is_empty());
    }

    #[test]
    fn config_space_is_the_mac() {
        let net = Net::new(FakeTap::default(), [2, 3, 4, 5, 6, 7]);
        assert_eq!(net.config(), &[2, 3, 4, 5, 6, 7]);
        assert_eq!(net.features(), (1 << 32) | (1 << 5));
    }
}
