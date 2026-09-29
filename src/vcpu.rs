//! The vCPU run loop: enter the guest with KVM_RUN and dispatch each VM exit. In `boot`
//! mode each vCPU runs on its own thread; `stop_all` stops and joins them.

use std::os::unix::thread::JoinHandleExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kvm_ioctls::{VcpuExit, VcpuFd};

use crate::devices::mmio_bus::MmioBus;
use crate::devices::pio_bus::{PioAction, PioBus};
use crate::error::{Error, Result};
use crate::event_loop::VmExit;

/// Why the guest stopped running normally.
#[derive(Debug, PartialEq, Eq)]
pub enum GuestStop {
    /// The guest executed `HLT` (only reported to user space without an in-kernel irqchip).
    Halted,
    /// KVM_EXIT_SHUTDOWN: triple fault or similar.
    Shutdown,
    /// The guest asked a device (the i8042) for a system reset.
    Reset,
    /// The VMM set the stop flag (another thread already decided the VM's outcome).
    Stopped,
}

/// Everything a vCPU dispatches exits to.
pub struct Buses<'a> {
    pub pio: &'a Mutex<PioBus>,
    pub mmio: &'a MmioBus,
}

/// Run the vCPU until the guest halts, resets or shuts down, or `stop` is set.
pub fn run(vcpu: &mut VcpuFd, buses: &Buses, stop: &AtomicBool) -> Result<GuestStop> {
    loop {
        // Checked before every KVM_RUN; a kick (SIGRTMIN) makes a running KVM_RUN
        // return EINTR so we get back here.
        if stop.load(Ordering::Acquire) {
            return Ok(GuestStop::Stopped);
        }
        // KVM_RUN: execute guest code until something needs the VMM's attention.
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            // A signal interrupted KVM_RUN, or KVM asked us to retry: re-check `stop`.
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => continue,
            Err(source) => {
                return Err(Error::Kvm {
                    op: "KVM_RUN",
                    source,
                })
            }
        };
        match exit {
            VcpuExit::IoOut(port, data) => {
                let action = buses
                    .pio
                    .lock()
                    .map_err(|_| Error::LockPoisoned("port I/O bus"))?
                    .write(port, data)?;
                if action == PioAction::Reset {
                    return Ok(GuestStop::Reset);
                }
            }
            VcpuExit::IoIn(port, data) => buses
                .pio
                .lock()
                .map_err(|_| Error::LockPoisoned("port I/O bus"))?
                .read(port, data)?,
            VcpuExit::MmioRead(addr, data) => buses.mmio.read(addr, data)?,
            VcpuExit::MmioWrite(addr, data) => buses.mmio.write(addr, data)?,
            VcpuExit::Hlt => return Ok(GuestStop::Halted),
            VcpuExit::Shutdown => return Ok(GuestStop::Shutdown),
            other => return Err(Error::UnexpectedExit(format!("{other:?}"))),
        }
    }
}

/// The signal used to kick vCPU threads out of KVM_RUN.
fn kick_signal() -> libc::c_int {
    libc::SIGRTMIN()
}

extern "C" fn on_kick(_signal: libc::c_int) {}

/// Install a no-op handler for the kick signal, without `SA_RESTART`, so a kick makes
/// a blocked KVM_RUN return `EINTR`. Must run before any vCPU thread starts.
pub fn install_kick_handler() -> Result<()> {
    // SAFETY: `sigaction` is plain data, so all-zero is a valid starting value (empty
    // mask, no flags: in particular no SA_RESTART).
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_kick as extern "C" fn(libc::c_int) as usize;
    // SAFETY: `action` is fully initialized and points at an `extern "C"` handler that
    // does nothing (trivially async-signal-safe); we don't need the old action.
    let ret = unsafe { libc::sigaction(kick_signal(), &action, std::ptr::null_mut()) };
    if ret != 0 {
        return Err(Error::Thread(format!(
            "sigaction(SIGRTMIN) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Records an outcome if the vCPU thread ends without one (e.g. it panicked), so the
/// event loop always wakes up. `VmExit::finish` ignores it if an outcome exists.
struct ExitGuard(Arc<VmExit>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0
            .finish(Err(Error::Thread("vCPU thread ended unexpectedly".into())));
    }
}

/// State shared by all vCPU threads.
#[derive(Clone)]
pub struct Shared {
    pub pio: Arc<Mutex<PioBus>>,
    pub mmio: Arc<MmioBus>,
    pub stop: Arc<AtomicBool>,
    pub exit: Arc<VmExit>,
}

/// Start vCPU `index` on its own thread, named `vcpu<index>`.
pub fn spawn(index: usize, mut vcpu: VcpuFd, shared: Shared) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("vcpu{index}"))
        .spawn(move || {
            let _guard = ExitGuard(shared.exit.clone());
            let buses = Buses {
                pio: &shared.pio,
                mmio: &shared.mmio,
            };
            let outcome = match run(&mut vcpu, &buses, &shared.stop) {
                Ok(GuestStop::Reset | GuestStop::Shutdown) => Ok(()),
                // Someone else decided the outcome; nothing to report.
                Ok(GuestStop::Stopped) => return,
                // With an in-kernel irqchip KVM handles HLT itself; this should not happen.
                Ok(GuestStop::Halted) => Err(Error::UnexpectedExit("HLT".into())),
                Err(e) => Err(e),
            };
            shared.exit.finish(outcome);
        })
        .map_err(|e| Error::Thread(e.to_string()))
}

/// How often unfinished vCPU threads are kicked while stopping.
const KICK_INTERVAL: Duration = Duration::from_millis(10);
/// How long vCPU threads get to stop before the VMM gives up with an error.
const STOP_DEADLINE: Duration = Duration::from_secs(5);

/// Stop every vCPU thread and join it. Sets `stop`, then kicks each unfinished thread
/// every 10 ms (a kick that lands just before KVM_RUN is lost; the next one isn't).
pub fn stop_all(handles: Vec<JoinHandle<()>>, stop: &AtomicBool) -> Result<()> {
    stop.store(true, Ordering::Release);
    let deadline = Instant::now() + STOP_DEADLINE;
    while handles.iter().any(|h| !h.is_finished()) {
        if Instant::now() > deadline {
            return Err(Error::Thread(format!(
                "vCPU threads did not stop within {} s",
                STOP_DEADLINE.as_secs()
            )));
        }
        for handle in handles.iter().filter(|h| !h.is_finished()) {
            // SAFETY: the pthread_t comes from a JoinHandle that has not been joined or
            // detached, so it still names a valid thread (possibly one that just exited,
            // which pthread_kill tolerates). The signal has a no-op handler.
            unsafe { libc::pthread_kill(handle.as_pthread_t(), kick_signal()) };
        }
        thread::sleep(KICK_INTERVAL);
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| Error::Thread("vCPU thread panicked".into()))?;
    }
    Ok(())
}
