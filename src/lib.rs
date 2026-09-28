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
pub mod vcpu;
pub mod vm;

use std::io::Write;
use std::path::PathBuf;

pub use error::{Error, Result};

use devices::pio_bus::PioBus;
use devices::serial::{NoopTrigger, SerialDevice, COM1_BASE, COM1_LEN};
use kvm_bindings::kvm_sregs;
use vcpu::GuestStop;

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
    arch::x86_64::regs::setup_cpuid(&vm.kvm, &vcpu)?;
    match config.mode {
        RawMode::Real => arch::x86_64::regs::setup_real_mode(&vcpu, entry)?,
        RawMode::Long => arch::x86_64::regs::setup_long_mode(&vcpu, &vm.memory, entry)?,
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
    }
}
