//! `vmm` command-line entry point: parse arguments, run the library, map errors to exit codes.

#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use kvm_hello::{run_boot, run_raw, BootConfig, RawConfig, RawMode, DEFAULT_CMDLINE};

#[derive(Parser)]
#[command(
    name = "vmm",
    version,
    about = "Minimal KVM-based virtual machine monitor"
)]
struct Cli {
    /// Path of the KVM device to open.
    #[arg(long, global = true, hide = true, default_value = "/dev/kvm")]
    kvm_device: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a flat binary guest loaded at guest-physical 0x1000.
    Raw {
        /// Flat binary image to load.
        #[arg(long)]
        image: PathBuf,
        /// CPU mode the guest starts in.
        #[arg(long, value_enum)]
        mode: Mode,
        /// Guest RAM in MiB.
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..=3072))]
        mem_mib: u64,
    },
    /// Boot a Linux bzImage with a serial console on stdin/stdout.
    Boot {
        /// Linux kernel in bzImage format.
        #[arg(long)]
        kernel: PathBuf,
        /// Initramfs (cpio archive).
        #[arg(long)]
        initrd: Option<PathBuf>,
        /// Guest RAM in MiB.
        #[arg(long, default_value_t = 512, value_parser = clap::value_parser!(u64).range(128..=3072))]
        mem_mib: u64,
        /// Kernel command line. The VMM appends `virtio_mmio.device=` entries for
        /// the devices it attaches.
        #[arg(long, default_value = DEFAULT_CMDLINE)]
        cmdline: String,
        /// Disk image for the virtio-blk device (/dev/vda in the guest).
        #[arg(long)]
        disk: Option<PathBuf>,
        /// Attach the disk read-only.
        #[arg(long, requires = "disk")]
        disk_ro: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Real,
    Long,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // Invalid arguments make clap exit with code 2, as the spec requires.
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Raw {
            image,
            mode,
            mem_mib,
        } => {
            let config = RawConfig {
                kvm_device: cli.kvm_device,
                image,
                mode: match mode {
                    Mode::Real => RawMode::Real,
                    Mode::Long => RawMode::Long,
                },
                mem_mib,
            };
            run_raw(&config, std::io::stdout()).map(|_| ())
        }
        Command::Boot {
            kernel,
            initrd,
            mem_mib,
            cmdline,
            disk,
            disk_ro,
        } => run_boot(&BootConfig {
            kvm_device: cli.kvm_device,
            kernel,
            initrd,
            mem_mib,
            cmdline,
            disk,
            disk_ro,
        }),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vmm: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}
