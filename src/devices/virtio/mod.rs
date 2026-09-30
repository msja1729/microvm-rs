//! virtio devices over the virtio-mmio transport.
//!
//! - `queue` and `parse` are pure, guest-facing code over the `GuestMem` trait (no KVM).
//! - `mmio` is the register interface the guest driver talks to.
//! - `block` executes parsed requests against the host file; `net` moves frames
//!   between the queues and a host TAP.

pub mod block;
pub mod mmio;
pub mod net;
pub mod parse;
#[cfg(kani)]
mod proofs;
pub mod queue;

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use queue::{range_in_memory, GuestMem, MemError, Queue, QueueError};

/// Production guest memory: the VM's single RAM region, starting at guest-physical 0.
#[derive(Clone)]
pub struct GuestRam {
    memory: GuestMemoryMmap,
    size: u64,
}

impl GuestRam {
    /// `memory` must map exactly `[0, size)`, which is how `vm::create` builds it.
    pub fn new(memory: GuestMemoryMmap, size: u64) -> Self {
        Self { memory, size }
    }
}

impl GuestMem for GuestRam {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), MemError> {
        if !range_in_memory(self.size, addr, buf.len() as u64) {
            return Err(MemError);
        }
        self.memory
            .read_slice(buf, GuestAddress(addr))
            .map_err(|_| MemError)
    }

    fn write(&self, addr: u64, buf: &[u8]) -> Result<(), MemError> {
        if !range_in_memory(self.size, addr, buf.len() as u64) {
            return Err(MemError);
        }
        self.memory
            .write_slice(buf, GuestAddress(addr))
            .map_err(|_| MemError)
    }
}

/// `InterruptStatus` bit: the device used buffers in a queue.
pub const INT_VRING: u32 = virtio_bindings::virtio_mmio::VIRTIO_MMIO_INT_VRING;
/// `InterruptStatus` bit: the device configuration (here: device status) changed.
pub const INT_CONFIG: u32 = virtio_bindings::virtio_mmio::VIRTIO_MMIO_INT_CONFIG;

/// A device's interrupt line: the `InterruptStatus` bits plus the irqfd that raises the
/// guest IRQ. Shared, lock-free, between the vCPU threads (status reads and ACKs) and
/// the event loop (raising interrupts), so neither ever waits for the other's I/O.
pub struct Interrupt {
    status: AtomicU32,
    irqfd: EventFd,
}

impl Interrupt {
    /// `irqfd` must already be registered with `KVM_IRQFD` on the device's GSI.
    pub fn new(irqfd: EventFd) -> Self {
        Self {
            status: AtomicU32::new(0),
            irqfd,
        }
    }

    /// Set `bits` in `InterruptStatus`, then send an edge to the guest. The status is
    /// set first, so the handler the edge triggers always sees why it was raised.
    pub fn signal(&self, bits: u32) -> io::Result<()> {
        self.status.fetch_or(bits, Ordering::SeqCst);
        self.irqfd.write(1)
    }

    /// Current `InterruptStatus`.
    pub fn status(&self) -> u32 {
        self.status.load(Ordering::SeqCst)
    }

    /// `InterruptACK`: the driver clears the bits it has handled.
    pub fn ack(&self, bits: u32) {
        self.status.fetch_and(!bits, Ordering::SeqCst);
    }

    /// Device reset: no interrupt is pending.
    pub fn clear(&self) {
        self.status.store(0, Ordering::SeqCst);
    }
}

/// A virtio device type behind the virtio-mmio transport.
pub trait VirtioDevice: Send {
    /// virtio device ID (`DeviceID` register).
    fn device_type(&self) -> u32;
    /// Feature bits the device offers.
    fn features(&self) -> u64;
    /// Number of virtqueues.
    fn num_queues(&self) -> usize;
    /// Device-specific configuration space (read-only for the guest).
    fn config(&self) -> &[u8];
    /// Handle a notification on queue `index`: consume every available chain and
    /// return whether any buffer was used (so the transport raises an interrupt).
    /// `QueueError` means the ring is corrupt and the queue must stop.
    fn process_queue<M: GuestMem>(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &M,
    ) -> Result<bool, QueueError>;
}
