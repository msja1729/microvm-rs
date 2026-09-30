//! 16550 UART (COM1) backed by `vm-superio`, plus the host-stdin reader that feeds it.

use std::convert::Infallible;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::warn;
use seccompiler::BpfProgram;
use vm_superio::serial::{Error as SerialError, NoEvents};
use vm_superio::{Serial, Trigger};
use vmm_sys_util::eventfd::EventFd;

use super::pio_bus::{PioAction, PioDevice};
use crate::error::{Error, Result};
use crate::event_loop::VmExit;

/// First I/O port of COM1.
pub const COM1_BASE: u16 = 0x3f8;
/// COM1 occupies eight consecutive ports.
pub const COM1_LEN: u16 = 8;
/// COM1's legacy interrupt line.
pub const COM1_IRQ: u32 = 4;

/// Interrupt trigger that does nothing. Used in `raw` mode, which has no irqchip.
pub struct NoopTrigger;

impl Trigger for NoopTrigger {
    type E = Infallible;

    fn trigger(&self) -> std::result::Result<(), Infallible> {
        Ok(())
    }
}

/// Raises the UART's IRQ by signalling an eventfd registered with `KVM_IRQFD`,
/// so KVM injects the interrupt without a round trip through the vCPU thread.
pub struct EventFdTrigger(pub EventFd);

impl Trigger for EventFdTrigger {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        self.0.write(1)
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

    /// Queue host input for the guest. Returns how many bytes fit in the RX FIFO
    /// (0 when it is full; the caller retries later).
    pub fn enqueue_input(&mut self, bytes: &[u8]) -> Result<usize> {
        match self.serial.enqueue_raw_bytes(bytes) {
            Ok(n) => Ok(n),
            Err(SerialError::FullFifo) => Ok(0),
            Err(e) => Err(Error::SerialInput(format!("{e:?}"))),
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

impl<T: Trigger + Send, W: Write + Send> PioDevice for SerialDevice<T, W> {
    fn read(&mut self, offset: u16, data: &mut [u8]) -> Result<()> {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = match register(offset, i) {
                Some(reg) => self.serial.read(reg),
                None => 0xff,
            };
        }
        Ok(())
    }

    fn write(&mut self, offset: u16, data: &[u8]) -> Result<PioAction> {
        for (i, &byte) in data.iter().enumerate() {
            if let Some(reg) = register(offset, i) {
                self.serial
                    .write(reg, byte)
                    .map_err(|e| Error::SerialOutput(format!("{e:?}")))?;
            }
        }
        Ok(PioAction::Continue)
    }
}

/// A serial device shared between the vCPU thread (guest port I/O) and the stdin thread.
pub struct SharedSerial<T: Trigger, W: Write>(pub Arc<Mutex<SerialDevice<T, W>>>);

impl<T: Trigger + Send, W: Write + Send> PioDevice for SharedSerial<T, W> {
    fn read(&mut self, offset: u16, data: &mut [u8]) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| Error::LockPoisoned("serial"))?
            .read(offset, data)
    }

    fn write(&mut self, offset: u16, data: &[u8]) -> Result<PioAction> {
        self.0
            .lock()
            .map_err(|_| Error::LockPoisoned("serial"))?
            .write(offset, data)
    }
}

/// How long the stdin thread waits before retrying when the guest's RX FIFO is full.
const FIFO_FULL_RETRY: Duration = Duration::from_millis(1);

/// Start a thread that copies host stdin into the guest's UART. Input is never dropped:
/// when the 64-byte RX FIFO is full the thread waits for the guest to drain it.
/// On EOF or a read error the thread stops; the guest keeps running.
/// The thread is never joined: at VM exit it may still be blocked in `read`.
/// With `seccomp`, the thread installs that filter before its first read; if that
/// fails, it reports the error through `exit` and reads nothing.
pub fn spawn_stdin_reader<T, W>(
    serial: Arc<Mutex<SerialDevice<T, W>>>,
    seccomp: Option<BpfProgram>,
    exit: Arc<VmExit>,
) -> Result<()>
where
    T: Trigger + Send + 'static,
    W: Write + Send + 'static,
{
    thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            if let Some(filter) = &seccomp {
                if let Err(e) = crate::seccomp::install(filter) {
                    exit.finish(Err(e));
                    return;
                }
            }
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 64];
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        warn!("stopped reading stdin: {e}");
                        return;
                    }
                };
                let mut pending = buf.get(..n).unwrap_or_default();
                while !pending.is_empty() {
                    let accepted = match serial.lock() {
                        Ok(mut s) => match s.enqueue_input(pending) {
                            Ok(accepted) => accepted,
                            Err(e) => {
                                warn!("stopped forwarding stdin: {e}");
                                return;
                            }
                        },
                        // The vCPU thread reports the poisoned lock itself.
                        Err(_) => return,
                    };
                    pending = pending.get(accepted..).unwrap_or_default();
                    if accepted == 0 {
                        thread::sleep(FIFO_FULL_RETRY);
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|e| Error::Thread(e.to_string()))
}
