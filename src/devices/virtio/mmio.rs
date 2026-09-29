//! virtio-mmio transport, register layout version 2 (virtio 1.x §4.2.2).
//!
//! Register accesses arrive on vCPU threads. `QueueNotify` writes normally never get
//! here: KVM delivers them to per-queue ioeventfds that the event loop watches, and the
//! event loop calls `VirtioMmio::process_queue`. Both sides share the transport behind
//! a `Mutex`; `InterruptStatus`/`InterruptACK` bypass the lock (see `Interrupt`).

use std::sync::{Arc, Mutex};

use log::{debug, warn};
use virtio_bindings::virtio_config::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_CONFIG_S_NEEDS_RESET,
    VIRTIO_F_VERSION_1,
};

use super::queue::{Queue, QueueError, MAX_QUEUE_SIZE};
use super::{GuestRam, Interrupt, VirtioDevice, INT_CONFIG, INT_VRING};
use crate::devices::mmio_bus::MmioDevice;
use crate::devices::virtio::queue::GuestMem;
use crate::error::{Error, Result};

/// "virt" in little-endian ASCII.
const MAGIC_VALUE: u32 = 0x7472_6976;
/// Register layout version 2 (non-legacy).
const VERSION: u32 = 2;
/// "QEMU": the vendor ID Linux and most VMMs use for virtio-mmio devices.
const VENDOR_ID: u32 = 0x554d_4551;

// Register offsets (virtio 1.x Table 4.1).
const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00c;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
/// `QueueNotify`; the ioeventfds are registered at `base + NOTIFY_OFFSET`.
pub const NOTIFY_OFFSET: u64 = 0x050;
pub const REG_INTERRUPT_STATUS: u64 = 0x060;
pub const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_DESC_HIGH: u64 = 0x084;
const REG_QUEUE_DRIVER_LOW: u64 = 0x090;
const REG_QUEUE_DRIVER_HIGH: u64 = 0x094;
const REG_QUEUE_DEVICE_LOW: u64 = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const REG_CONFIG_GENERATION: u64 = 0x0fc;
/// Device-specific configuration space starts here.
const CONFIG_SPACE: u64 = 0x100;

/// The transport state of one virtio-mmio device.
pub struct VirtioMmio<D: VirtioDevice> {
    device: D,
    mem: GuestRam,
    interrupt: Arc<Interrupt>,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    status: u32,
    queue_sel: u32,
    queues: Vec<Queue>,
    /// `DRIVER_OK` was accepted: the event loop may process the queues.
    active: bool,
}

impl<D: VirtioDevice> VirtioMmio<D> {
    pub fn new(device: D, mem: GuestRam, interrupt: Arc<Interrupt>) -> Self {
        let queues = vec![Queue::default(); device.num_queues()];
        Self {
            device,
            mem,
            interrupt,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            status: 0,
            queue_sel: 0,
            queues,
            active: false,
        }
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    fn reset(&mut self) {
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.status = 0;
        self.queue_sel = 0;
        self.queues.fill(Queue::default());
        self.active = false;
        self.interrupt.clear();
    }

    fn selected_queue(&self) -> Option<&Queue> {
        self.queues.get(usize::try_from(self.queue_sel).ok()?)
    }

    /// The selected queue, if it exists and its registers may still be written: queue
    /// registers are frozen while the queue is ready or the device is running.
    fn writable_queue(&mut self) -> Option<&mut Queue> {
        if self.active {
            return None;
        }
        let queue = self.queues.get_mut(usize::try_from(self.queue_sel).ok()?)?;
        (!queue.ready).then_some(queue)
    }

    /// The 32-bit half of a 64-bit feature word chosen by a `*FeaturesSel` register.
    fn feature_word(features: u64, sel: u32) -> u32 {
        match sel {
            0 => features as u32,
            1 => (features >> 32) as u32,
            _ => 0,
        }
    }

    /// Handle a read of `data.len()` bytes at `offset` into the MMIO window.
    pub fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= CONFIG_SPACE {
            self.read_config(offset - CONFIG_SPACE, data);
            return;
        }
        if data.len() != 4 || !offset.is_multiple_of(4) {
            debug!("virtio-mmio: bad read width/alignment at {offset:#x}");
            data.fill(0xff);
            return;
        }
        let value = match offset {
            REG_MAGIC => MAGIC_VALUE,
            REG_VERSION => VERSION,
            REG_DEVICE_ID => self.device.device_type(),
            REG_VENDOR_ID => VENDOR_ID,
            REG_DEVICE_FEATURES => {
                Self::feature_word(self.device.features(), self.device_features_sel)
            }
            REG_QUEUE_NUM_MAX => match self.selected_queue() {
                Some(_) => u32::from(MAX_QUEUE_SIZE),
                None => 0,
            },
            REG_QUEUE_READY => self.selected_queue().map_or(0, |q| u32::from(q.ready)),
            REG_INTERRUPT_STATUS => self.interrupt.status(),
            REG_STATUS => self.status,
            REG_CONFIG_GENERATION => 0,
            _ => {
                debug!("virtio-mmio: read of write-only or unknown register {offset:#x}");
                0
            }
        };
        data.copy_from_slice(&value.to_le_bytes());
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.device.config();
        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(data.len())?));
        match range.and_then(|r| config.get(r)) {
            Some(bytes) if matches!(data.len(), 1 | 2 | 4 | 8) => data.copy_from_slice(bytes),
            _ => {
                debug!("virtio-mmio: config read outside config space at {offset:#x}");
                data.fill(0xff);
            }
        }
    }

    /// Handle a write of `data` at `offset` into the MMIO window.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= CONFIG_SPACE {
            debug!("virtio-mmio: ignored config write at {offset:#x}");
            return;
        }
        let Ok(bytes) = <[u8; 4]>::try_from(data) else {
            debug!("virtio-mmio: bad write width at {offset:#x}");
            return;
        };
        if !offset.is_multiple_of(4) {
            debug!("virtio-mmio: misaligned write at {offset:#x}");
            return;
        }
        let value = u32::from_le_bytes(bytes);
        let low = |old: u64| (old & !0xffff_ffff) | u64::from(value);
        let high = |old: u64| (old & 0xffff_ffff) | (u64::from(value) << 32);
        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES => {
                // Features are fixed once the driver sets FEATURES_OK.
                if self.status & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
                    match self.driver_features_sel {
                        0 => self.driver_features = low(self.driver_features),
                        1 => self.driver_features = high(self.driver_features),
                        _ => {}
                    }
                }
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM => {
                if let Some(q) = self.writable_queue() {
                    // Truncation is fine: any value above MAX_QUEUE_SIZE fails validation
                    // at DRIVER_OK anyway, and so does its truncated form unless it is a
                    // valid size.
                    q.size = u16::try_from(value).unwrap_or(0);
                }
            }
            REG_QUEUE_READY => {
                if !self.active {
                    let sel = usize::try_from(self.queue_sel).unwrap_or(usize::MAX);
                    if let Some(q) = self.queues.get_mut(sel) {
                        q.ready = value == 1;
                    }
                }
            }
            NOTIFY_OFFSET => {
                // Matching values go to the ioeventfds; anything that gets here names
                // no queue.
                debug!("virtio-mmio: ignored QueueNotify({value})");
            }
            REG_INTERRUPT_ACK => self.interrupt.ack(value),
            REG_STATUS => self.write_status(value),
            REG_QUEUE_DESC_LOW => self.set_queue_addr(|q| &mut q.desc_table, low),
            REG_QUEUE_DESC_HIGH => self.set_queue_addr(|q| &mut q.desc_table, high),
            REG_QUEUE_DRIVER_LOW => self.set_queue_addr(|q| &mut q.avail_ring, low),
            REG_QUEUE_DRIVER_HIGH => self.set_queue_addr(|q| &mut q.avail_ring, high),
            REG_QUEUE_DEVICE_LOW => self.set_queue_addr(|q| &mut q.used_ring, low),
            REG_QUEUE_DEVICE_HIGH => self.set_queue_addr(|q| &mut q.used_ring, high),
            _ => debug!("virtio-mmio: write to read-only or unknown register {offset:#x}"),
        }
    }

    fn set_queue_addr(&mut self, field: fn(&mut Queue) -> &mut u64, update: impl Fn(u64) -> u64) {
        if let Some(q) = self.writable_queue() {
            let addr = field(q);
            *addr = update(*addr);
        }
    }

    fn write_status(&mut self, value: u32) {
        if value == 0 {
            self.reset();
            return;
        }
        let mut value = value;
        let newly_set = value & !self.status;
        if newly_set & VIRTIO_CONFIG_S_FEATURES_OK != 0 {
            let offered = self.device.features();
            let version_1 = 1u64 << VIRTIO_F_VERSION_1;
            if self.driver_features & !offered != 0 || self.driver_features & version_1 == 0 {
                // The driver reads Status back and gives up when FEATURES_OK is missing.
                value &= !VIRTIO_CONFIG_S_FEATURES_OK;
            }
        }
        if newly_set & VIRTIO_CONFIG_S_DRIVER_OK != 0 {
            let mem_size = self.mem.size();
            let queues_ok = self
                .queues
                .iter()
                .filter(|q| q.ready)
                .all(|q| q.is_valid(mem_size));
            if value & VIRTIO_CONFIG_S_FEATURES_OK != 0 && queues_ok {
                self.active = true;
            } else {
                warn!("virtio-mmio: DRIVER_OK with invalid features or queues; device needs reset");
                value |= VIRTIO_CONFIG_S_NEEDS_RESET;
                self.raise(INT_CONFIG);
            }
        }
        // NEEDS_RESET belongs to the device; only a reset (Status = 0) clears it.
        self.status = value | (self.status & VIRTIO_CONFIG_S_NEEDS_RESET);
    }

    fn raise(&self, bits: u32) {
        if let Err(e) = self.interrupt.signal(bits) {
            warn!("virtio-mmio: cannot raise interrupt: {e}");
        }
    }

    /// Called by the event loop when queue `index` is notified. Does nothing unless the
    /// device is running and the queue is ready.
    pub fn process_queue(&mut self, index: usize) {
        if !self.active {
            return;
        }
        let Some(queue) = self.queues.get_mut(index).filter(|q| q.ready) else {
            return;
        };
        match self.device.process_queue(index, queue, &self.mem) {
            Ok(true) => self.raise(INT_VRING),
            Ok(false) => {}
            Err(QueueError::Broken) => {
                warn!("virtio-mmio: queue {index} is corrupt; device needs reset");
                self.active = false;
                self.status |= VIRTIO_CONFIG_S_NEEDS_RESET;
                // Buffers used before the corruption was found still need their interrupt.
                self.raise(INT_VRING | INT_CONFIG);
            }
        }
    }
}

/// A transport shared by the vCPU threads (register accesses) and the event loop.
pub type SharedTransport<D> = Arc<Mutex<VirtioMmio<D>>>;

/// What the MMIO bus holds for a virtio device: the shared transport, plus a lock-free
/// path for the interrupt registers so the guest's interrupt handler never waits for a
/// disk or network operation the event loop is doing under the lock.
pub struct MmioTransport<D: VirtioDevice> {
    transport: Arc<Mutex<VirtioMmio<D>>>,
    interrupt: Arc<Interrupt>,
}

impl<D: VirtioDevice> MmioTransport<D> {
    pub fn new(transport: Arc<Mutex<VirtioMmio<D>>>, interrupt: Arc<Interrupt>) -> Self {
        Self {
            transport,
            interrupt,
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, VirtioMmio<D>>> {
        self.transport
            .lock()
            .map_err(|_| Error::LockPoisoned("virtio transport"))
    }
}

impl<D: VirtioDevice> MmioDevice for MmioTransport<D> {
    fn read(&self, offset: u64, data: &mut [u8]) -> Result<()> {
        if offset == REG_INTERRUPT_STATUS && data.len() == 4 {
            data.copy_from_slice(&self.interrupt.status().to_le_bytes());
            return Ok(());
        }
        self.lock()?.read(offset, data);
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        if offset == REG_INTERRUPT_ACK {
            if let Ok(bytes) = <[u8; 4]>::try_from(data) {
                self.interrupt.ack(u32::from_le_bytes(bytes));
                return Ok(());
            }
        }
        self.lock()?.write(offset, data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio_bindings::virtio_config::{VIRTIO_CONFIG_S_ACKNOWLEDGE, VIRTIO_CONFIG_S_DRIVER};
    use vm_memory::{GuestAddress, GuestMemoryMmap};
    use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

    const MEM_SIZE: u64 = 0x10_0000;

    /// A device with two queues and a 6-byte config space that counts notifications.
    struct Dummy {
        processed: Vec<usize>,
        result: std::result::Result<bool, QueueError>,
    }

    impl VirtioDevice for Dummy {
        fn device_type(&self) -> u32 {
            42
        }
        fn features(&self) -> u64 {
            (1 << VIRTIO_F_VERSION_1) | 0b101
        }
        fn num_queues(&self) -> usize {
            2
        }
        fn config(&self) -> &[u8] {
            &[1, 2, 3, 4, 5, 6]
        }
        fn process_queue<M: GuestMem>(
            &mut self,
            index: usize,
            _queue: &mut Queue,
            _mem: &M,
        ) -> std::result::Result<bool, QueueError> {
            self.processed.push(index);
            self.result
        }
    }

    fn transport() -> (VirtioMmio<Dummy>, Arc<Interrupt>) {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE as usize)])
            .expect("anonymous mapping");
        let interrupt = Arc::new(Interrupt::new(EventFd::new(EFD_NONBLOCK).expect("eventfd")));
        let dummy = Dummy {
            processed: Vec::new(),
            result: Ok(true),
        };
        (
            VirtioMmio::new(dummy, GuestRam::new(memory, MEM_SIZE), interrupt.clone()),
            interrupt,
        )
    }

    fn r32(t: &mut VirtioMmio<Dummy>, offset: u64) -> u32 {
        let mut b = [0u8; 4];
        t.read(offset, &mut b);
        u32::from_le_bytes(b)
    }

    fn w32(t: &mut VirtioMmio<Dummy>, offset: u64, value: u32) {
        t.write(offset, &value.to_le_bytes());
    }

    /// Run the Linux driver's init sequence with valid queues; return the transport.
    fn negotiate(t: &mut VirtioMmio<Dummy>, driver_features: u64) {
        w32(t, REG_STATUS, VIRTIO_CONFIG_S_ACKNOWLEDGE);
        w32(
            t,
            REG_STATUS,
            VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER,
        );
        w32(t, REG_DRIVER_FEATURES_SEL, 1);
        w32(t, REG_DRIVER_FEATURES, (driver_features >> 32) as u32);
        w32(t, REG_DRIVER_FEATURES_SEL, 0);
        w32(t, REG_DRIVER_FEATURES, driver_features as u32);
        let s = VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER | VIRTIO_CONFIG_S_FEATURES_OK;
        w32(t, REG_STATUS, s);
    }

    fn setup_queue(t: &mut VirtioMmio<Dummy>, index: u32, base: u64) {
        w32(t, REG_QUEUE_SEL, index);
        w32(t, REG_QUEUE_NUM, 16);
        w32(t, REG_QUEUE_DESC_LOW, base as u32);
        w32(t, REG_QUEUE_DRIVER_LOW, (base + 0x100) as u32);
        w32(t, REG_QUEUE_DEVICE_LOW, (base + 0x200) as u32);
        w32(t, REG_QUEUE_READY, 1);
    }

    #[test]
    fn identification_registers() {
        let (mut t, _) = transport();
        assert_eq!(r32(&mut t, REG_MAGIC), 0x7472_6976);
        assert_eq!(r32(&mut t, REG_VERSION), 2);
        assert_eq!(r32(&mut t, REG_DEVICE_ID), 42);
        assert_eq!(r32(&mut t, REG_VENDOR_ID), VENDOR_ID);
        assert_eq!(r32(&mut t, REG_CONFIG_GENERATION), 0);
        w32(&mut t, REG_DEVICE_FEATURES_SEL, 0);
        assert_eq!(r32(&mut t, REG_DEVICE_FEATURES), 0b101);
        w32(&mut t, REG_DEVICE_FEATURES_SEL, 1);
        assert_eq!(r32(&mut t, REG_DEVICE_FEATURES), 1);
        w32(&mut t, REG_DEVICE_FEATURES_SEL, 2);
        assert_eq!(r32(&mut t, REG_DEVICE_FEATURES), 0);
        w32(&mut t, REG_QUEUE_SEL, 1);
        assert_eq!(r32(&mut t, REG_QUEUE_NUM_MAX), 256);
        w32(&mut t, REG_QUEUE_SEL, 2);
        assert_eq!(r32(&mut t, REG_QUEUE_NUM_MAX), 0);
    }

    #[test]
    fn bad_widths_and_config_space() {
        let (mut t, _) = transport();
        let mut two = [0u8; 2];
        t.read(REG_MAGIC, &mut two);
        assert_eq!(two, [0xff, 0xff]);
        let mut four = [0u8; 4];
        t.read(REG_MAGIC + 1, &mut four);
        assert_eq!(four, [0xff; 4]);

        t.read(CONFIG_SPACE + 2, &mut four);
        assert_eq!(four, [3, 4, 5, 6]);
        t.read(CONFIG_SPACE + 4, &mut four);
        assert_eq!(four, [0xff; 4], "crosses the end of config space");
        let mut three = [0u8; 3];
        t.read(CONFIG_SPACE, &mut three);
        assert_eq!(three, [0xff; 3], "3-byte access");
        t.read(u64::MAX - 1, &mut two);
        assert_eq!(two, [0xff; 2]);
        // A 2-byte status write is ignored.
        t.write(REG_STATUS, &[1, 0]);
        assert_eq!(r32(&mut t, REG_STATUS), 0);
    }

    #[test]
    fn feature_negotiation_accepts_subset_with_version_1() {
        let (mut t, _) = transport();
        negotiate(&mut t, (1 << VIRTIO_F_VERSION_1) | 0b1);
        assert_ne!(r32(&mut t, REG_STATUS) & VIRTIO_CONFIG_S_FEATURES_OK, 0);
    }

    #[test]
    fn feature_negotiation_rejects_unoffered_or_legacy() {
        let (mut t, _) = transport();
        negotiate(&mut t, (1 << VIRTIO_F_VERSION_1) | 0b10);
        assert_eq!(r32(&mut t, REG_STATUS) & VIRTIO_CONFIG_S_FEATURES_OK, 0);
        let (mut t, _) = transport();
        negotiate(&mut t, 0b1);
        assert_eq!(r32(&mut t, REG_STATUS) & VIRTIO_CONFIG_S_FEATURES_OK, 0);
    }

    #[test]
    fn driver_ok_activates_and_notifications_are_processed() {
        let (mut t, irq) = transport();
        negotiate(&mut t, 1 << VIRTIO_F_VERSION_1);
        t.process_queue(0);
        assert!(
            t.device().processed.is_empty(),
            "not active before DRIVER_OK"
        );
        setup_queue(&mut t, 0, 0x1000);
        let s = r32(&mut t, REG_STATUS);
        w32(&mut t, REG_STATUS, s | VIRTIO_CONFIG_S_DRIVER_OK);
        assert!(t.is_active());
        t.process_queue(0);
        t.process_queue(1); // not ready: ignored
        t.process_queue(7); // no such queue: ignored
        assert_eq!(t.device().processed, vec![0]);
        assert_eq!(r32(&mut t, REG_INTERRUPT_STATUS), INT_VRING);
        w32(&mut t, REG_INTERRUPT_ACK, INT_VRING);
        assert_eq!(irq.status(), 0);
    }

    #[test]
    fn queue_registers_frozen_while_ready() {
        let (mut t, _) = transport();
        setup_queue(&mut t, 0, 0x1000);
        w32(&mut t, REG_QUEUE_NUM, 8);
        w32(&mut t, REG_QUEUE_DESC_LOW, 0x5000);
        assert_eq!(t.queues[0].size, 16);
        assert_eq!(t.queues[0].desc_table, 0x1000);
        assert_eq!(r32(&mut t, REG_QUEUE_READY), 1);
    }

    #[test]
    fn invalid_queue_sets_needs_reset() {
        let (mut t, irq) = transport();
        negotiate(&mut t, 1 << VIRTIO_F_VERSION_1);
        setup_queue(&mut t, 0, MEM_SIZE - 0x100); // rings run past the end of RAM
        let s = r32(&mut t, REG_STATUS);
        w32(&mut t, REG_STATUS, s | VIRTIO_CONFIG_S_DRIVER_OK);
        assert!(!t.is_active());
        assert_ne!(r32(&mut t, REG_STATUS) & VIRTIO_CONFIG_S_NEEDS_RESET, 0);
        assert_eq!(irq.status(), INT_CONFIG);
    }

    #[test]
    fn broken_queue_stops_device() {
        let (mut t, irq) = transport();
        negotiate(&mut t, 1 << VIRTIO_F_VERSION_1);
        setup_queue(&mut t, 0, 0x1000);
        let s = r32(&mut t, REG_STATUS);
        w32(&mut t, REG_STATUS, s | VIRTIO_CONFIG_S_DRIVER_OK);
        t.device.result = Err(QueueError::Broken);
        t.process_queue(0);
        assert!(!t.is_active());
        assert_ne!(r32(&mut t, REG_STATUS) & VIRTIO_CONFIG_S_NEEDS_RESET, 0);
        assert_eq!(irq.status(), INT_VRING | INT_CONFIG);
        t.process_queue(0);
        assert_eq!(
            t.device().processed,
            vec![0],
            "no processing after the queue broke"
        );
    }

    #[test]
    fn reset_clears_everything() {
        let (mut t, irq) = transport();
        negotiate(&mut t, 1 << VIRTIO_F_VERSION_1);
        setup_queue(&mut t, 0, 0x1000);
        let s = r32(&mut t, REG_STATUS);
        w32(&mut t, REG_STATUS, s | VIRTIO_CONFIG_S_DRIVER_OK);
        t.process_queue(0);
        w32(&mut t, REG_STATUS, 0);
        assert!(!t.is_active());
        assert_eq!(r32(&mut t, REG_STATUS), 0);
        assert_eq!(irq.status(), 0);
        assert_eq!(t.queues[0], Queue::default());
        assert_eq!(t.driver_features, 0);
    }

    #[test]
    fn mmio_transport_interrupt_fast_path() {
        let (t, irq) = transport();
        let shared = MmioTransport::new(Arc::new(Mutex::new(t)), irq.clone());
        irq.signal(INT_VRING | INT_CONFIG).expect("eventfd write");
        let mut b = [0u8; 4];
        shared.read(REG_INTERRUPT_STATUS, &mut b).expect("read");
        assert_eq!(u32::from_le_bytes(b), 3);
        shared
            .write(REG_INTERRUPT_ACK, &1u32.to_le_bytes())
            .expect("write");
        assert_eq!(irq.status(), INT_CONFIG);
        shared.read(REG_MAGIC, &mut b).expect("read");
        assert_eq!(u32::from_le_bytes(b), MAGIC_VALUE);
    }
}
