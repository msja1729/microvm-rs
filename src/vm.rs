//! VM creation: open the KVM device, check capabilities, create the VM and its guest
//! memory; attach virtio-mmio devices.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use kvm_bindings::{kvm_pit_config, kvm_userspace_memory_region};
use kvm_ioctls::{Cap, IoEventAddress, Kvm, VmFd};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

use crate::arch::x86_64::{KVM_TSS_ADDRESS, MMIO_WINDOW_SIZE};
use crate::devices::mmio_bus::MmioBus;
use crate::devices::virtio::mmio::{MmioTransport, SharedTransport, VirtioMmio, NOTIFY_OFFSET};
use crate::devices::virtio::{GuestRam, Interrupt, VirtioDevice};
use crate::error::{Error, Result};

/// The only KVM API version that has ever been stable.
const KVM_API_VERSION: i32 = 12;
/// Largest guest RAM size Phase 1 accepts; keeps RAM below the 32-bit MMIO hole.
pub const MAX_MEM_MIB: u64 = 3072;

pub struct Vm {
    pub kvm: Kvm,
    pub fd: VmFd,
    pub memory: GuestMemoryMmap,
}

/// Open `kvm_device`, then create a VM with `mem_mib` MiB of RAM mapped at guest-physical 0.
/// No irqchip or PIT is created: this is the setup for `raw` mode.
pub fn create(kvm_device: &Path, mem_mib: u64, min_mem_mib: u64) -> Result<Vm> {
    if !(min_mem_mib..=MAX_MEM_MIB).contains(&mem_mib) {
        return Err(Error::InvalidMemSize {
            mib: mem_mib,
            min: min_mem_mib,
            max: MAX_MEM_MIB,
        });
    }

    let c_path = CString::new(kvm_device.as_os_str().as_bytes())
        .map_err(|_| Error::KvmPathNul(kvm_device.to_path_buf()))?;
    let kvm = Kvm::new_with_path(&c_path).map_err(|source| Error::KvmOpen {
        path: kvm_device.to_path_buf(),
        source,
    })?;

    // KVM_GET_API_VERSION: refuse to run against an incompatible kernel interface.
    let version = kvm.get_api_version();
    if version != KVM_API_VERSION {
        return Err(Error::KvmApiVersion(version));
    }
    // KVM_CHECK_EXTENSION: we register guest RAM with KVM_SET_USER_MEMORY_REGION.
    if !kvm.check_extension(Cap::UserMemory) {
        return Err(Error::MissingCapability("KVM_CAP_USER_MEMORY"));
    }

    // KVM_CREATE_VM: a new, empty VM with no memory and no vCPUs.
    let fd = kvm.create_vm().map_err(|source| Error::Kvm {
        op: "KVM_CREATE_VM",
        source,
    })?;

    // KVM_SET_TSS_ADDR: on Intel, KVM needs three pages of guest-physical address space
    // for a TSS used to emulate real mode. It must be set before the first KVM_RUN.
    fd.set_tss_address(KVM_TSS_ADDRESS)
        .map_err(|source| Error::Kvm {
            op: "KVM_SET_TSS_ADDR",
            source,
        })?;

    // The range check above bounds this to 3 GiB, so neither conversion can fail.
    let size = mem_mib
        .checked_mul(1 << 20)
        .and_then(|b| usize::try_from(b).ok())
        .ok_or_else(|| Error::GuestMemory(format!("{mem_mib} MiB does not fit in usize")))?;
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size)])
        .map_err(|e| Error::GuestMemory(e.to_string()))?;
    let host_addr = memory
        .get_host_address(GuestAddress(0))
        .map_err(|e| Error::GuestMemory(e.to_string()))?;

    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: size as u64,
        userspace_addr: host_addr as u64,
    };
    // KVM_SET_USER_MEMORY_REGION: back guest-physical [0, size) with our host mapping.
    // SAFETY: `host_addr..host_addr + size` is a single live mmap owned by `memory`, which
    // is stored in the returned `Vm` next to `fd`, so the mapping outlives the VM's use of it.
    // It does not overlap any other memory slot (this is the only one).
    unsafe { fd.set_user_memory_region(region) }.map_err(|source| Error::Kvm {
        op: "KVM_SET_USER_MEMORY_REGION",
        source,
    })?;

    Ok(Vm { kvm, fd, memory })
}

impl Vm {
    /// Create the in-kernel PIC/IOAPIC/LAPIC and the in-kernel PIT (`boot` mode only).
    /// Must run before any vCPU is created, so each vCPU gets an in-kernel LAPIC.
    pub fn setup_irqchip(&self) -> Result<()> {
        if !self.kvm.check_extension(Cap::Irqchip) {
            return Err(Error::MissingCapability("KVM_CAP_IRQCHIP"));
        }
        if !self.kvm.check_extension(Cap::Irqfd) {
            return Err(Error::MissingCapability("KVM_CAP_IRQFD"));
        }
        // KVM_CREATE_IRQCHIP: emulate the PICs, IOAPIC and LAPICs in the kernel, with
        // default routing of GSIs 0-15 to both the PIC and the IOAPIC.
        self.fd.create_irq_chip().map_err(|source| Error::Kvm {
            op: "KVM_CREATE_IRQCHIP",
            source,
        })?;
        // KVM_CREATE_PIT2: an in-kernel 8254 timer on ports 0x40-0x43 feeding IRQ 0.
        self.fd
            .create_pit2(kvm_pit_config::default())
            .map_err(|source| Error::Kvm {
                op: "KVM_CREATE_PIT2",
                source,
            })
    }

    /// Attach a virtio-mmio device in the window at `base`, interrupting on `gsi`:
    /// - an irqfd on `gsi` (edge: each eventfd write is one pulse);
    /// - one ioeventfd per queue on `QueueNotify`, matching the queue index, so a
    ///   notification never exits to user space;
    /// - the transport on `mmio` for every other register access.
    ///
    /// Returns the shared transport and the queue eventfds (index = queue), which the
    /// event loop watches. Must run before any vCPU starts.
    pub fn attach_virtio<D: VirtioDevice + 'static>(
        &self,
        mmio: &mut MmioBus,
        base: u64,
        gsi: u32,
        device: D,
        mem: GuestRam,
    ) -> Result<(SharedTransport<D>, Vec<EventFd>)> {
        let eventfd = || {
            EventFd::new(EFD_NONBLOCK).map_err(|e| Error::Kvm {
                op: "eventfd",
                source: e.into(),
            })
        };
        let irqfd = eventfd()?;
        // KVM_IRQFD: a write to `irqfd` raises GSI `gsi` in the in-kernel irqchip
        // (PIC and IOAPIC, through KVM's default routing for GSIs 0-15).
        self.fd
            .register_irqfd(&irqfd, gsi)
            .map_err(|source| Error::Kvm {
                op: "KVM_IRQFD",
                source,
            })?;

        let mut queue_evts = Vec::with_capacity(device.num_queues());
        for index in 0..device.num_queues() {
            let evt = eventfd()?;
            let queue =
                u32::try_from(index).map_err(|_| Error::UnexpectedExit("queue index".into()))?;
            // KVM_IOEVENTFD: a 4-byte guest write of `queue` to QueueNotify signals
            // `evt` inside KVM instead of exiting to user space. A u32 datamatch makes
            // the registration 4 bytes long with data matching enabled.
            self.fd
                .register_ioevent(&evt, &IoEventAddress::Mmio(base + NOTIFY_OFFSET), queue)
                .map_err(|source| Error::Kvm {
                    op: "KVM_IOEVENTFD",
                    source,
                })?;
            queue_evts.push(evt);
        }

        let interrupt = Arc::new(Interrupt::new(irqfd));
        let transport = Arc::new(Mutex::new(VirtioMmio::new(device, mem, interrupt.clone())));
        mmio.insert(
            base,
            MMIO_WINDOW_SIZE,
            Arc::new(MmioTransport::new(transport.clone(), interrupt)),
        );
        Ok((transport, queue_evts))
    }
}
