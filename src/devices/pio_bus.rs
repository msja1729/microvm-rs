//! Port-I/O dispatch. Accesses to ports no device claims read as all-ones and
//! ignore writes, because Linux probes many legacy ports while booting.

use log::debug;

use crate::error::Result;

/// A device that owns a contiguous range of I/O ports.
pub trait PioDevice {
    /// Handle an `IN` at `offset` bytes into the device's range; fill all of `data`.
    fn read(&mut self, offset: u16, data: &mut [u8]);
    /// Handle an `OUT` at `offset` bytes into the device's range.
    fn write(&mut self, offset: u16, data: &[u8]) -> Result<()>;
}

struct Entry {
    base: u16,
    len: u16,
    device: Box<dyn PioDevice>,
}

#[derive(Default)]
pub struct PioBus {
    entries: Vec<Entry>,
}

impl PioBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `device` for ports `base..base + len`.
    pub fn insert(&mut self, base: u16, len: u16, device: Box<dyn PioDevice>) {
        self.entries.push(Entry { base, len, device });
    }

    fn find(&mut self, port: u16) -> Option<(u16, &mut Box<dyn PioDevice>)> {
        self.entries.iter_mut().find_map(|e| {
            let offset = port.checked_sub(e.base)?;
            (offset < e.len).then_some((offset, &mut e.device))
        })
    }

    pub fn read(&mut self, port: u16, data: &mut [u8]) {
        match self.find(port) {
            Some((offset, device)) => device.read(offset, data),
            None => {
                debug!("unhandled port read: port={port:#x} size={}", data.len());
                data.fill(0xff);
            }
        }
    }

    pub fn write(&mut self, port: u16, data: &[u8]) -> Result<()> {
        match self.find(port) {
            Some((offset, device)) => device.write(offset, data),
            None => {
                debug!("unhandled port write: port={port:#x} data={data:02x?}");
                Ok(())
            }
        }
    }
}
