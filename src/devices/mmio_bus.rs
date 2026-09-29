//! MMIO dispatch for `KVM_EXIT_MMIO`. Unclaimed addresses read as all-ones and
//! ignore writes, like unclaimed I/O ports.
//!
//! The bus is built before any vCPU runs and never changes afterwards, so vCPU threads
//! share it without a lock. Each device does its own locking (`&self` methods).

use std::sync::Arc;

use log::debug;

use crate::error::Result;

/// A device that owns a range of guest-physical MMIO addresses.
pub trait MmioDevice: Send + Sync {
    /// Handle a read at `offset` bytes into the device's range; fill all of `data`.
    fn read(&self, offset: u64, data: &mut [u8]) -> Result<()>;
    /// Handle a write at `offset` bytes into the device's range.
    fn write(&self, offset: u64, data: &[u8]) -> Result<()>;
}

struct Entry {
    base: u64,
    len: u64,
    device: Arc<dyn MmioDevice>,
}

#[derive(Default)]
pub struct MmioBus {
    entries: Vec<Entry>,
}

impl MmioBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `device` for addresses `base..base + len`.
    pub fn insert(&mut self, base: u64, len: u64, device: Arc<dyn MmioDevice>) {
        self.entries.push(Entry { base, len, device });
    }

    fn find(&self, addr: u64) -> Option<(u64, &Arc<dyn MmioDevice>)> {
        self.entries.iter().find_map(|e| {
            let offset = addr.checked_sub(e.base)?;
            (offset < e.len).then_some((offset, &e.device))
        })
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) -> Result<()> {
        match self.find(addr) {
            Some((offset, device)) => device.read(offset, data),
            None => {
                debug!("unhandled MMIO read: addr={addr:#x} size={}", data.len());
                data.fill(0xff);
                Ok(())
            }
        }
    }

    pub fn write(&self, addr: u64, data: &[u8]) -> Result<()> {
        match self.find(addr) {
            Some((offset, device)) => device.write(offset, data),
            None => {
                debug!("unhandled MMIO write: addr={addr:#x} data={data:02x?}");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u64, Vec<u8>)>>);

    impl MmioDevice for Recorder {
        fn read(&self, offset: u64, data: &mut [u8]) -> Result<()> {
            data.fill(offset as u8);
            Ok(())
        }
        fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
            self.0.lock().expect("lock").push((offset, data.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn dispatches_by_offset_and_defaults_to_all_ones() {
        let dev = Arc::new(Recorder::default());
        let mut bus = MmioBus::new();
        bus.insert(0xd000_0000, 0x1000, dev.clone());

        let mut data = [0u8; 4];
        bus.read(0xd000_0010, &mut data).expect("read");
        assert_eq!(data, [0x10; 4]);
        bus.read(0xd000_1000, &mut data).expect("read");
        assert_eq!(data, [0xff; 4], "one past the window");
        bus.read(0, &mut data).expect("read");
        assert_eq!(data, [0xff; 4]);

        bus.write(0xd000_0fff, &[7]).expect("write");
        bus.write(0xcfff_ffff, &[8]).expect("write");
        assert_eq!(*dev.0.lock().expect("lock"), vec![(0xfff, vec![7])]);
    }
}
