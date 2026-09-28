//! 16550 UART (COM1) backed by `vm-superio`.

use std::convert::Infallible;
use std::io::Write;

use vm_superio::serial::NoEvents;
use vm_superio::{Serial, Trigger};

use super::pio_bus::PioDevice;
use crate::error::{Error, Result};

/// First I/O port of COM1.
pub const COM1_BASE: u16 = 0x3f8;
/// COM1 occupies eight consecutive ports.
pub const COM1_LEN: u16 = 8;

/// Interrupt trigger that does nothing. Used in `raw` mode, which has no irqchip.
pub struct NoopTrigger;

impl Trigger for NoopTrigger {
    type E = Infallible;

    fn trigger(&self) -> std::result::Result<(), Infallible> {
        Ok(())
    }
}

pub struct SerialDevice<T: Trigger, W: Write> {
    serial: Serial<T, NoEvents, W>,
}

impl<T: Trigger, W: Write> SerialDevice<T, W> {
    pub fn new(trigger: T, out: W) -> Self {
        Self {
            serial: Serial::new(trigger, out),
        }
    }
}

/// Register index for byte `i` of an access at `offset`, if it is inside the UART.
/// A multi-byte access touches consecutive registers, as on real hardware.
fn register(offset: u16, i: usize) -> Option<u8> {
    let reg = usize::from(offset).checked_add(i)?;
    if reg < usize::from(COM1_LEN) {
        u8::try_from(reg).ok()
    } else {
        None
    }
}

impl<T: Trigger, W: Write> PioDevice for SerialDevice<T, W> {
    fn read(&mut self, offset: u16, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = match register(offset, i) {
                Some(reg) => self.serial.read(reg),
                None => 0xff,
            };
        }
    }

    fn write(&mut self, offset: u16, data: &[u8]) -> Result<()> {
        for (i, &byte) in data.iter().enumerate() {
            if let Some(reg) = register(offset, i) {
                self.serial
                    .write(reg, byte)
                    .map_err(|e| Error::SerialOutput(format!("{e:?}")))?;
            }
        }
        Ok(())
    }
}
