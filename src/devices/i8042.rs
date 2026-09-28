//! The minimum of an i8042 keyboard controller needed to see a guest reset request.
//!
//! Linux with `reboot=k` resets the machine by writing 0xFE ("pulse reset line")
//! to the command port. Reads return 0, meaning "no data, input buffer empty", so
//! the kernel's i8042 driver and its reboot path never wait on a stuck controller.

use super::pio_bus::{PioAction, PioDevice};
use crate::error::Result;

/// The controller's data port is 0x60 and its command/status port 0x64.
pub const I8042_BASE: u16 = 0x60;
pub const I8042_LEN: u16 = 5;

const DATA_OFFSET: u16 = 0;
const COMMAND_OFFSET: u16 = 4;
const CMD_RESET_CPU: u8 = 0xfe;

pub struct I8042;

impl PioDevice for I8042 {
    fn read(&mut self, offset: u16, data: &mut [u8]) -> Result<()> {
        // Ports 0x61..=0x63 are not the i8042's; they read as unclaimed ports.
        let value = match offset {
            DATA_OFFSET | COMMAND_OFFSET => 0x00,
            _ => 0xff,
        };
        data.fill(value);
        Ok(())
    }

    fn write(&mut self, offset: u16, data: &[u8]) -> Result<PioAction> {
        if offset == COMMAND_OFFSET && data.first() == Some(&CMD_RESET_CPU) {
            Ok(PioAction::Reset)
        } else {
            Ok(PioAction::Continue)
        }
    }
}
