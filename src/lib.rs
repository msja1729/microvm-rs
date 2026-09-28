//! Minimal KVM-based VMM. The `vmm` binary is a thin CLI over this library.

#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::unwrap_used,
    clippy::expect_used
)]

pub mod arch;
pub mod devices;
pub mod error;
pub mod loader;
pub mod terminal;
pub mod vcpu;
pub mod vm;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use error::{Error, Result};

use arch::x86_64::{boot_params, regs, BOOT_PARAMS_START, CMDLINE_MAX_SIZE, CMDLINE_START};
use devices::i8042::{I8042, I8042_BASE, I8042_LEN};
use devices::pio_bus::PioBus;
use devices::serial::{
    EventFdTrigger, NoopTrigger, SerialDevice, SharedSerial, COM1_BASE, COM1_IRQ, COM1_LEN,
};
use kvm_bindings::kvm_sregs;
use linux_loader::cmdline::Cmdline;
use vcpu::GuestStop;
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
pub fn run_raw<W: Write + 'static>(config: &RawConfig, out: W) -> Result<RawOutcome> {
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

    match vcpu::run(&mut vcpu, &mut bus)? {
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
}

/// Smallest guest RAM accepted when booting Linux.
pub const BOOT_MIN_MEM_MIB: u64 = 128;
/// Kernel command line used when `--cmdline` is not given.
pub const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 pci=off acpi=off";

/// Boot a Linux bzImage with an interactive serial console on stdin/stdout.
/// Returns `Ok` when the guest resets or shuts down.
pub fn run_boot(config: &BootConfig) -> Result<()> {
    let vm = vm::create(&config.kvm_device, config.mem_mib, BOOT_MIN_MEM_MIB)?;
    // The irqchip must exist before the vCPU is created.
    vm.setup_irqchip()?;
    let mem_size = config.mem_mib << 20;

    let kernel = loader::load_kernel(&vm.memory, &config.kernel)?;
    let initrd = match &config.initrd {
        Some(path) => Some(loader::load_initrd(&vm.memory, path, &kernel, mem_size)?),
        None => None,
    };

    if config.cmdline.len() >= CMDLINE_MAX_SIZE {
        return Err(Error::Cmdline(format!(
            "{} bytes; at most {} allowed",
            config.cmdline.len(),
            CMDLINE_MAX_SIZE - 1
        )));
    }
    let mut cmdline = Cmdline::new(CMDLINE_MAX_SIZE).map_err(|e| Error::Cmdline(e.to_string()))?;
    cmdline
        .insert_str(&config.cmdline)
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
    let mut vcpu = vm.fd.create_vcpu(0).map_err(|source| Error::Kvm {
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

    let mut bus = PioBus::new();
    bus.insert(COM1_BASE, COM1_LEN, Box::new(SharedSerial(serial.clone())));
    bus.insert(I8042_BASE, I8042_LEN, Box::new(I8042));

    terminal::install_signal_handlers()?;
    // Dropped at the end of this function on every path, restoring the terminal.
    let _raw_mode = terminal::enter_raw_mode()?;
    devices::serial::spawn_stdin_reader(serial)?;

    match vcpu::run(&mut vcpu, &mut bus)? {
        GuestStop::Reset | GuestStop::Shutdown => Ok(()),
        // With an in-kernel irqchip KVM handles HLT itself; this should not happen.
        GuestStop::Halted => Err(Error::UnexpectedExit("HLT".into())),
    }
}
