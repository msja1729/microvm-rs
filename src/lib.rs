//! Minimal KVM-based VMM. The `vmm` binary is a thin CLI over this library.

#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Unit tests may panic on setup failures; production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod arch;
pub mod devices;
pub mod error;
pub mod event_loop;
pub mod loader;
pub mod terminal;
pub mod vcpu;
pub mod vm;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub use error::{Error, Result};

use arch::x86_64::{
    boot_params, regs, BLK_IRQ, BLK_MMIO_BASE, BOOT_PARAMS_START, CMDLINE_MAX_SIZE, CMDLINE_START,
    MMIO_WINDOW_SIZE, NET_IRQ, NET_MMIO_BASE,
};
use devices::i8042::{I8042, I8042_BASE, I8042_LEN};
use devices::mmio_bus::MmioBus;
use devices::pio_bus::PioBus;
use devices::serial::{
    EventFdTrigger, NoopTrigger, SerialDevice, SharedSerial, COM1_BASE, COM1_IRQ, COM1_LEN,
};
use devices::virtio::block::Block;
use devices::virtio::net::{Net, DEFAULT_MAC};
use devices::virtio::parse::SECTOR_SIZE;
use devices::virtio::GuestRam;
use event_manager::SubscriberOps;
use kvm_bindings::kvm_sregs;
use linux_loader::cmdline::Cmdline;
use vcpu::{Buses, GuestStop};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

/// CPU mode a raw guest starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawMode {
    /// 16-bit real mode.
    Real,
    /// 64-bit long mode with the first 1 GiB identity-mapped.
    Long,
}

/// Configuration for `vmm raw`.
#[derive(Debug, Clone)]
pub struct RawConfig {
    pub kvm_device: PathBuf,
    pub image: PathBuf,
    pub mode: RawMode,
    pub mem_mib: u64,
}

/// State of a raw guest after it halted.
#[derive(Debug, Clone)]
pub struct RawOutcome {
    /// The vCPU's special registers (segments, control registers, EFER) at the halt.
    pub sregs: kvm_sregs,
}

/// Smallest guest RAM accepted in raw mode.
pub const RAW_MIN_MEM_MIB: u64 = 1;

/// Run a flat raw guest until it executes `HLT`. Serial output is written to `out`.
pub fn run_raw<W: Write + Send + 'static>(config: &RawConfig, out: W) -> Result<RawOutcome> {
    let vm = vm::create(&config.kvm_device, config.mem_mib, RAW_MIN_MEM_MIB)?;
    let entry = loader::load_raw_image(&vm.memory, &config.image)?;

    // KVM_CREATE_VCPU: the single vCPU, id 0. No irqchip exists, so HLT exits to us.
    let mut vcpu = vm.fd.create_vcpu(0).map_err(|source| Error::Kvm {
        op: "KVM_CREATE_VCPU",
        source,
    })?;
    regs::setup_cpuid(&vm.kvm, &vcpu)?;
    match config.mode {
        RawMode::Real => regs::setup_real_mode(&vcpu, entry)?,
        RawMode::Long => regs::setup_long_mode(&vcpu, &vm.memory, entry, 0)?,
    }

    let mut bus = PioBus::new();
    bus.insert(
        COM1_BASE,
        COM1_LEN,
        Box::new(SerialDevice::new(NoopTrigger, out)),
    );

    let buses = Buses {
        pio: &Mutex::new(bus),
        mmio: &MmioBus::new(),
    };
    // Raw mode runs its single vCPU on the calling thread; nothing ever sets `stop`.
    match vcpu::run(&mut vcpu, &buses, &AtomicBool::new(false))? {
        GuestStop::Halted => {
            // KVM_GET_SREGS: report the final CPU mode to the caller.
            let sregs = vcpu.get_sregs().map_err(|source| Error::Kvm {
                op: "KVM_GET_SREGS",
                source,
            })?;
            Ok(RawOutcome { sregs })
        }
        GuestStop::Shutdown => Err(Error::GuestShutdown),
        // No i8042 is attached in raw mode, so nothing can request a reset.
        GuestStop::Reset => Err(Error::UnexpectedExit("reset request".into())),
        GuestStop::Stopped => Err(Error::UnexpectedExit("vCPU stopped".into())),
    }
}

/// Configuration for `vmm boot`.
#[derive(Debug, Clone)]
pub struct BootConfig {
    pub kvm_device: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub mem_mib: u64,
    pub cmdline: String,
    /// Backing file for the virtio-blk device, if any.
    pub disk: Option<PathBuf>,
    /// Open the disk read-only and advertise `VIRTIO_BLK_F_RO`.
    pub disk_ro: bool,
    /// Existing host TAP interface backing the virtio-net device, if any.
    pub tap: Option<String>,
    /// Guest MAC address; `DEFAULT_MAC` when `None`.
    pub guest_mac: Option<[u8; 6]>,
}

/// Smallest guest RAM accepted when booting Linux.
pub const BOOT_MIN_MEM_MIB: u64 = 128;
/// Kernel command line used when `--cmdline` is not given.
pub const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 pci=off acpi=off";

/// A disk image checked to be usable as a virtio-blk backing file.
struct Disk {
    file: File,
    size: u64,
}

/// Open the disk image read-write (or read-only) and check it is a regular file whose
/// size is a non-zero multiple of 512 bytes.
fn open_disk(path: &Path, read_only: bool) -> Result<Disk> {
    let disk_error = |reason: String| Error::Disk {
        path: path.to_path_buf(),
        reason,
    };
    let file = OpenOptions::new()
        .read(true)
        .write(!read_only)
        .open(path)
        .map_err(|e| disk_error(e.to_string()))?;
    let meta = file.metadata().map_err(|e| disk_error(e.to_string()))?;
    if !meta.is_file() {
        return Err(disk_error("not a regular file".into()));
    }
    let size = meta.len();
    if size == 0 || size % SECTOR_SIZE != 0 {
        return Err(disk_error(format!(
            "size is {size} bytes; it must be a non-zero multiple of {SECTOR_SIZE}"
        )));
    }
    Ok(Disk { file, size })
}

/// `virtio_mmio.device=<size>@<base>:<irq>`: how the guest kernel learns about a
/// virtio-mmio device when there is no ACPI or device tree.
fn virtio_mmio_param(base: u64, irq: u32) -> String {
    format!(
        "virtio_mmio.device={}K@{base:#x}:{irq}",
        MMIO_WINDOW_SIZE / 1024
    )
}

/// The user's command line followed by the entries for the configured devices. The
/// length limit applies to this final string.
fn final_cmdline(user: &str, has_disk: bool, has_net: bool) -> Result<String> {
    let mut parts = vec![user.to_owned()];
    if has_disk {
        parts.push(virtio_mmio_param(BLK_MMIO_BASE, BLK_IRQ));
    }
    if has_net {
        parts.push(virtio_mmio_param(NET_MMIO_BASE, NET_IRQ));
    }
    parts.retain(|p| !p.is_empty());
    let cmdline = parts.join(" ");
    if cmdline.len() >= CMDLINE_MAX_SIZE {
        return Err(Error::Cmdline(format!(
            "{} bytes including the VMM's device entries; at most {} allowed",
            cmdline.len(),
            CMDLINE_MAX_SIZE - 1
        )));
    }
    Ok(cmdline)
}

/// Boot a Linux bzImage with an interactive serial console on stdin/stdout.
/// Returns `Ok` when the guest resets or shuts down.
///
/// Threads: the vCPU runs on its own thread (`vcpu0`); this thread runs the event loop
/// (virtio queue notifications, VM exit) and, when the VM ends, stops and joins the vCPU.
pub fn run_boot(config: &BootConfig) -> Result<()> {
    // Host-side inputs are checked before anything else, so their errors (exit 2) don't
    // depend on the kernel or KVM.
    let disk = match &config.disk {
        Some(path) => Some(open_disk(path, config.disk_ro)?),
        None => None,
    };
    let tap = match &config.tap {
        Some(name) => Some(devices::tap::open(name)?),
        None => None,
    };
    let cmdline_str = final_cmdline(&config.cmdline, disk.is_some(), tap.is_some())?;

    let vm = vm::create(&config.kvm_device, config.mem_mib, BOOT_MIN_MEM_MIB)?;
    // The irqchip must exist before the vCPU is created.
    vm.setup_irqchip()?;
    let mem_size = config.mem_mib << 20;

    let kernel = loader::load_kernel(&vm.memory, &config.kernel)?;
    let initrd = match &config.initrd {
        Some(path) => Some(loader::load_initrd(&vm.memory, path, &kernel, mem_size)?),
        None => None,
    };

    let mut cmdline = Cmdline::new(CMDLINE_MAX_SIZE).map_err(|e| Error::Cmdline(e.to_string()))?;
    cmdline
        .insert_str(&cmdline_str)
        .map_err(|e| Error::Cmdline(e.to_string()))?;
    linux_loader::loader::load_cmdline(
        &vm.memory,
        vm_memory::GuestAddress(CMDLINE_START),
        &cmdline,
    )
    .map_err(|e| Error::GuestMemory(e.to_string()))?;

    let params = boot_params::build(kernel.header, CMDLINE_START, initrd, mem_size)?;
    boot_params::write(&vm.memory, &params)?;

    // KVM_CREATE_VCPU: the single vCPU, id 0; it gets an in-kernel LAPIC.
    let vcpu = vm.fd.create_vcpu(0).map_err(|source| Error::Kvm {
        op: "KVM_CREATE_VCPU",
        source,
    })?;
    regs::setup_cpuid(&vm.kvm, &vcpu)?;
    regs::setup_long_mode(&vcpu, &vm.memory, kernel.entry, BOOT_PARAMS_START)?;

    let irq_evt = EventFd::new(EFD_NONBLOCK).map_err(|e| Error::Kvm {
        op: "eventfd",
        source: e.into(),
    })?;
    // KVM_IRQFD: a write to `irq_evt` raises GSI 4 (COM1) in the in-kernel irqchip.
    vm.fd
        .register_irqfd(&irq_evt, COM1_IRQ)
        .map_err(|source| Error::Kvm {
            op: "KVM_IRQFD",
            source,
        })?;
    let serial = Arc::new(Mutex::new(SerialDevice::new(
        EventFdTrigger(irq_evt),
        std::io::stdout(),
    )));

    let mut pio = PioBus::new();
    pio.insert(COM1_BASE, COM1_LEN, Box::new(SharedSerial(serial.clone())));
    pio.insert(I8042_BASE, I8042_LEN, Box::new(I8042));

    let exit = event_loop::VmExit::new()?;
    let mut events = event_loop::new(&exit)?;
    let mut mmio = MmioBus::new();
    if let Some(disk) = disk {
        let ram = GuestRam::new(vm.memory.clone(), mem_size);
        let block = Block::new(disk.file, disk.size, config.disk_ro);
        let (transport, queue_evts) =
            vm.attach_virtio(&mut mmio, BLK_MMIO_BASE, BLK_IRQ, block, ram)?;
        events.add_subscriber(Box::new(event_loop::QueueSubscriber::new(
            transport,
            queue_evts,
            exit.clone(),
        )));
    }
    if let Some(tap) = tap {
        use std::os::unix::io::AsRawFd;
        let tap_fd = tap.as_raw_fd();
        let ram = GuestRam::new(vm.memory.clone(), mem_size);
        let net = Net::new(tap, config.guest_mac.unwrap_or(DEFAULT_MAC));
        let (transport, queue_evts) =
            vm.attach_virtio(&mut mmio, NET_MMIO_BASE, NET_IRQ, net, ram)?;
        events.add_subscriber(Box::new(event_loop::NetSubscriber::new(
            transport,
            queue_evts,
            tap_fd,
            exit.clone(),
        )));
    }

    terminal::install_signal_handlers()?;
    vcpu::install_kick_handler()?;
    // Dropped at the end of this function on every path, restoring the terminal.
    let _raw_mode = terminal::enter_raw_mode()?;
    devices::serial::spawn_stdin_reader(serial)?;

    let shared = vcpu::Shared {
        pio: Arc::new(Mutex::new(pio)),
        mmio: Arc::new(mmio),
        stop: Arc::new(AtomicBool::new(false)),
        exit: exit.clone(),
    };
    let handles = vec![vcpu::spawn(0, vcpu, shared.clone())?];

    event_loop::run(&mut events, &exit);
    vcpu::stop_all(handles, &shared.stop)?;
    exit.take()
        .unwrap_or_else(|| Err(Error::Thread("VM ended without an outcome".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_gets_virtio_entry_only_with_a_disk() {
        assert_eq!(
            final_cmdline("console=ttyS0", false, false).expect("ok"),
            "console=ttyS0"
        );
        assert_eq!(
            final_cmdline("console=ttyS0", true, false).expect("ok"),
            "console=ttyS0 virtio_mmio.device=4K@0xd0000000:5"
        );
        assert_eq!(
            final_cmdline("", true, false).expect("ok"),
            "virtio_mmio.device=4K@0xd0000000:5"
        );
    }

    #[test]
    fn cmdline_gets_net_entry_after_disk() {
        assert_eq!(
            final_cmdline("c", true, true).expect("ok"),
            "c virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6"
        );
        assert_eq!(
            final_cmdline("c", false, true).expect("ok"),
            "c virtio_mmio.device=4K@0xd0001000:6"
        );
    }

    #[test]
    fn cmdline_limit_applies_to_final_string() {
        let entry_len = " virtio_mmio.device=4K@0xd0000000:5".len();
        let fits = "a".repeat(CMDLINE_MAX_SIZE - 1 - entry_len);
        assert!(final_cmdline(&fits, true, false).is_ok());
        let too_long = "a".repeat(CMDLINE_MAX_SIZE - entry_len);
        assert!(matches!(
            final_cmdline(&too_long, true, false),
            Err(Error::Cmdline(_))
        ));
        assert!(final_cmdline(&too_long, false, false).is_ok());
    }

    #[test]
    fn disk_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d.img");
        for (size, ok) in [
            (0u64, false),
            (511, false),
            (513, false),
            (512, true),
            (1 << 20, true),
        ] {
            let f = File::create(&path).expect("create");
            f.set_len(size).expect("set_len");
            let r = open_disk(&path, false);
            assert_eq!(r.is_ok(), ok, "size {size}");
            if let Err(e) = r {
                assert_eq!(e.exit_code(), 2);
                assert!(e.to_string().contains("d.img"));
            }
        }
        let missing = dir.path().join("missing.img");
        assert!(matches!(open_disk(&missing, true), Err(Error::Disk { .. })));
        assert!(matches!(
            open_disk(dir.path(), true),
            Err(Error::Disk { .. })
        ));
    }
}
