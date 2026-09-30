//! The main thread's event loop: virtio queue notifications (ioeventfds) and the VM
//! exit signal. Built on `event-manager` (epoll).

use std::io::{Read, Write};
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use event_manager::{EventManager, EventOps, EventSet, Events, MutEventSubscriber};
use log::warn;
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

use crate::devices::virtio::mmio::SharedTransport;
use crate::devices::virtio::net::{Net, RX_QUEUE};
use crate::devices::virtio::VirtioDevice;
use crate::error::{Error, Result};

/// How the VM ended, reported once by whichever thread saw it first (a vCPU seeing a
/// reset, a vCPU or the event loop failing), plus an eventfd that wakes the event loop.
pub struct VmExit {
    outcome: Mutex<Option<Result<()>>>,
    evt: EventFd,
}

impl VmExit {
    pub fn new() -> Result<Arc<Self>> {
        let evt =
            EventFd::new(EFD_NONBLOCK).map_err(|e| Error::EventLoop(format!("eventfd: {e}")))?;
        Ok(Arc::new(Self {
            outcome: Mutex::new(None),
            evt,
        }))
    }

    /// Record `result` unless an outcome is already recorded (first writer wins), and
    /// wake the event loop.
    pub fn finish(&self, result: Result<()>) {
        // A poisoned lock still holds a usable Option; keep going so we never lose an exit.
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if outcome.is_none() {
            *outcome = Some(result);
        }
        drop(outcome);
        if let Err(e) = self.evt.write(1) {
            warn!("cannot signal VM exit: {e}");
        }
    }

    pub fn is_finished(&self) -> bool {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// The recorded outcome (taken out; later calls return `None`).
    pub fn take(&self) -> Option<Result<()>> {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// Wakes the loop when `VmExit::finish` is called.
struct ExitSubscriber(Arc<VmExit>);

impl MutEventSubscriber for ExitSubscriber {
    fn init(&mut self, ops: &mut EventOps) {
        if let Err(e) = ops.add(Events::new(&self.0.evt, EventSet::IN)) {
            self.0
                .finish(Err(Error::EventLoop(format!("watch exit eventfd: {e}"))));
        }
    }

    fn process(&mut self, _events: Events, _ops: &mut EventOps) {
        // Drain the counter; `run` checks `is_finished` after every wakeup.
        let _ = self.0.evt.read();
    }
}

/// Serves the queue notifications of one virtio device. Event data = queue index.
pub struct QueueSubscriber<D: VirtioDevice> {
    transport: SharedTransport<D>,
    queue_evts: Vec<EventFd>,
    exit: Arc<VmExit>,
}

impl<D: VirtioDevice> QueueSubscriber<D> {
    /// `queue_evts[i]` must be the ioeventfd registered for queue `i`.
    pub fn new(transport: SharedTransport<D>, queue_evts: Vec<EventFd>, exit: Arc<VmExit>) -> Self {
        Self {
            transport,
            queue_evts,
            exit,
        }
    }
}

impl<D: VirtioDevice> MutEventSubscriber for QueueSubscriber<D> {
    fn init(&mut self, ops: &mut EventOps) {
        for (index, evt) in self.queue_evts.iter().enumerate() {
            let data = u32::try_from(index).unwrap_or(u32::MAX);
            if let Err(e) = ops.add(Events::with_data(evt, data, EventSet::IN)) {
                self.exit
                    .finish(Err(Error::EventLoop(format!("watch queue eventfd: {e}"))));
            }
        }
    }

    fn process(&mut self, events: Events, _ops: &mut EventOps) {
        let index = events.data() as usize;
        let Some(evt) = self.queue_evts.get(index) else {
            return;
        };
        // Reset the counter; several notifications collapse into one pass over the queue.
        let _ = evt.read();
        match self.transport.lock() {
            Ok(mut transport) => transport.process_queue(index),
            Err(_) => self
                .exit
                .finish(Err(Error::LockPoisoned("virtio transport"))),
        }
    }
}

/// Event data for the TAP descriptor (queue eventfds use their queue index).
const TAP_EVENT: u32 = 2;

/// Serves a virtio-net device: RX/TX queue notifications and a readable TAP.
///
/// The TAP is watched only while the RX queue may have buffers. epoll is level-
/// triggered, so watching a readable TAP with nowhere to put its frames would spin;
/// when an RX pass ends starved (or the device isn't running yet) the TAP is dropped
/// from the interest list, and it is added back when the guest notifies the RX queue
/// (which it does after adding buffers) and that RX pass isn't starved.
pub struct NetSubscriber<T: Read + Write + Send> {
    transport: SharedTransport<Net<T>>,
    queue_evts: Vec<EventFd>,
    /// Owned by the `Net` device inside `transport`, which this subscriber keeps alive.
    tap_fd: RawFd,
    tap_watched: bool,
    exit: Arc<VmExit>,
}

impl<T: Read + Write + Send> NetSubscriber<T> {
    /// `queue_evts[i]` must be the ioeventfd of queue `i`; `tap_fd` the device's TAP.
    pub fn new(
        transport: SharedTransport<Net<T>>,
        queue_evts: Vec<EventFd>,
        tap_fd: RawFd,
        exit: Arc<VmExit>,
    ) -> Self {
        Self {
            transport,
            queue_evts,
            tap_fd,
            tap_watched: false,
            exit,
        }
    }

    fn set_tap_watched(&mut self, ops: &mut EventOps, watch: bool) {
        if watch == self.tap_watched {
            return;
        }
        let events = Events::with_data_raw(self.tap_fd, TAP_EVENT, EventSet::IN);
        let result = if watch {
            ops.add(events)
        } else {
            ops.remove(events)
        };
        match result {
            Ok(()) => self.tap_watched = watch,
            Err(e) => self
                .exit
                .finish(Err(Error::EventLoop(format!("update TAP interest: {e}")))),
        }
    }
}

impl<T: Read + Write + Send> MutEventSubscriber for NetSubscriber<T> {
    fn init(&mut self, ops: &mut EventOps) {
        for (index, evt) in self.queue_evts.iter().enumerate() {
            let data = u32::try_from(index).unwrap_or(u32::MAX);
            if let Err(e) = ops.add(Events::with_data(evt, data, EventSet::IN)) {
                self.exit
                    .finish(Err(Error::EventLoop(format!("watch queue eventfd: {e}"))));
            }
        }
        self.set_tap_watched(ops, true);
    }

    fn process(&mut self, events: Events, ops: &mut EventOps) {
        let data = events.data();
        let queue = match data {
            TAP_EVENT => RX_QUEUE,
            index => {
                let Some(evt) = self.queue_evts.get(index as usize) else {
                    return;
                };
                let _ = evt.read();
                index as usize
            }
        };
        let (active, starved) = match self.transport.lock() {
            Ok(mut transport) => {
                transport.process_queue(queue);
                (transport.is_active(), transport.device().rx_starved())
            }
            Err(_) => {
                self.exit
                    .finish(Err(Error::LockPoisoned("virtio transport")));
                return;
            }
        };
        // An RX notification means the guest added buffers; a TAP event means frames
        // arrived. Either way the RX pass just ran: keep watching the TAP only if the
        // device is running and the pass did not run out of guest buffers.
        if queue == RX_QUEUE {
            self.set_tap_watched(ops, active && !starved);
        }
    }
}

pub type Subscribers = EventManager<Box<dyn MutEventSubscriber>>;

/// A new event loop that already watches `exit`.
pub fn new(exit: &Arc<VmExit>) -> Result<Subscribers> {
    use event_manager::SubscriberOps;
    let mut manager: Subscribers =
        EventManager::new().map_err(|e| Error::EventLoop(format!("epoll: {e}")))?;
    manager.add_subscriber(Box::new(ExitSubscriber(exit.clone())));
    Ok(manager)
}

/// Dispatch events until some thread records the VM's outcome.
pub fn run(manager: &mut Subscribers, exit: &VmExit) {
    while !exit.is_finished() {
        // Returns 0 when a signal interrupted epoll_wait; just check again.
        if let Err(e) = manager.run() {
            exit.finish(Err(Error::EventLoop(format!("epoll_wait: {e}"))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_outcome_wins_and_wakes_the_loop() {
        let exit = VmExit::new().expect("eventfd");
        let mut manager = new(&exit).expect("epoll");
        exit.finish(Ok(()));
        exit.finish(Err(Error::GuestShutdown));
        run(&mut manager, &exit); // returns at once: an outcome is recorded
        assert!(matches!(exit.take(), Some(Ok(()))));
        assert!(exit.take().is_none());
    }

    #[test]
    fn exit_from_another_thread_stops_the_loop() {
        let exit = VmExit::new().expect("eventfd");
        let mut manager = new(&exit).expect("epoll");
        let remote = exit.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            remote.finish(Err(Error::GuestShutdown));
        });
        run(&mut manager, &exit);
        t.join().expect("thread");
        assert!(matches!(exit.take(), Some(Err(Error::GuestShutdown))));
    }
}
