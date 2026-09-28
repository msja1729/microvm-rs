//! The vCPU run loop: enter the guest with KVM_RUN and dispatch each VM exit.

use kvm_ioctls::{VcpuExit, VcpuFd};
use log::debug;

use crate::devices::pio_bus::PioBus;
use crate::error::{Error, Result};

/// Why the guest stopped running normally.
#[derive(Debug, PartialEq, Eq)]
pub enum GuestStop {
    /// The guest executed `HLT` (only reported to user space without an in-kernel irqchip).
    Halted,
    /// KVM_EXIT_SHUTDOWN: triple fault or similar.
    Shutdown,
}

/// Run the vCPU until the guest halts or shuts down.
pub fn run(vcpu: &mut VcpuFd, bus: &mut PioBus) -> Result<GuestStop> {
    loop {
        // KVM_RUN: execute guest code until something needs the VMM's attention.
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            // A signal interrupted KVM_RUN, or KVM asked us to retry: just re-enter.
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => continue,
            Err(source) => {
                return Err(Error::Kvm {
                    op: "KVM_RUN",
                    source,
                })
            }
        };
        match exit {
            VcpuExit::IoOut(port, data) => bus.write(port, data)?,
            VcpuExit::IoIn(port, data) => bus.read(port, data),
            VcpuExit::MmioRead(addr, data) => {
                debug!("unhandled MMIO read: addr={addr:#x} size={}", data.len());
                data.fill(0xff);
            }
            VcpuExit::MmioWrite(addr, data) => {
                debug!("unhandled MMIO write: addr={addr:#x} data={data:02x?}");
            }
            VcpuExit::Hlt => return Ok(GuestStop::Halted),
            VcpuExit::Shutdown => return Ok(GuestStop::Shutdown),
            other => return Err(Error::UnexpectedExit(format!("{other:?}"))),
        }
    }
}
