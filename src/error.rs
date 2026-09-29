//! Error type for the whole VMM and its mapping to process exit codes.

use std::path::PathBuf;

/// Everything that can make the VMM stop with a non-zero exit code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // ---- Host setup errors (exit code 2) ----
    #[error("cannot open KVM device {}: {source}", path.display())]
    KvmOpen {
        path: PathBuf,
        source: kvm_ioctls::Error,
    },
    #[error("KVM device path {} contains a NUL byte", .0.display())]
    KvmPathNul(PathBuf),
    #[error("unsupported KVM API version {0} (expected 12)")]
    KvmApiVersion(i32),
    #[error("KVM capability {0} is not supported by this host")]
    MissingCapability(&'static str),
    #[error("guest memory size {mib} MiB is out of range ({min}..={max} MiB)")]
    InvalidMemSize { mib: u64, min: u64, max: u64 },
    #[error("cannot read image {}: {source}", path.display())]
    ImageRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("image {} is {size} bytes; it must be 1..={max} bytes", path.display())]
    ImageSize {
        path: PathBuf,
        size: usize,
        max: usize,
    },
    #[error("cannot read kernel {}: {source}", path.display())]
    KernelRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot load kernel {}: {reason}", path.display())]
    KernelLoad { path: PathBuf, reason: String },
    #[error("cannot read initrd {}: {source}", path.display())]
    InitrdRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("initrd {} ({size} bytes) does not fit in guest memory above the kernel", path.display())]
    InitrdTooLarge { path: PathBuf, size: u64 },
    #[error("invalid kernel command line: {0}")]
    Cmdline(String),
    #[error("cannot use disk image {}: {reason}", path.display())]
    Disk { path: PathBuf, reason: String },

    // ---- Unexpected VM exits and internal errors (exit code 1) ----
    #[error("KVM ioctl {op} failed: {source}")]
    Kvm {
        op: &'static str,
        source: kvm_ioctls::Error,
    },
    #[error("guest memory error: {0}")]
    GuestMemory(String),
    #[error("serial output error: {0}")]
    SerialOutput(String),
    #[error("serial input error: {0}")]
    SerialInput(String),
    #[error("{0} lock poisoned by a panicked thread")]
    LockPoisoned(&'static str),
    #[error("cannot start thread: {0}")]
    Thread(String),
    #[error("terminal error: {0}")]
    Terminal(String),
    #[error("unexpected VM exit: {0}")]
    UnexpectedExit(String),
    #[error("guest shut down unexpectedly (KVM_EXIT_SHUTDOWN, e.g. a triple fault)")]
    GuestShutdown,
    #[error("event loop error: {0}")]
    EventLoop(String),
}

impl Error {
    /// Process exit code for this error, as defined in the Phase 1 spec.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::KvmOpen { .. }
            | Error::KvmPathNul(_)
            | Error::KvmApiVersion(_)
            | Error::MissingCapability(_)
            | Error::InvalidMemSize { .. }
            | Error::ImageRead { .. }
            | Error::ImageSize { .. }
            | Error::KernelRead { .. }
            | Error::KernelLoad { .. }
            | Error::InitrdRead { .. }
            | Error::InitrdTooLarge { .. }
            | Error::Cmdline(_)
            | Error::Disk { .. } => 2,
            Error::Kvm { .. }
            | Error::GuestMemory(_)
            | Error::SerialOutput(_)
            | Error::SerialInput(_)
            | Error::LockPoisoned(_)
            | Error::Thread(_)
            | Error::Terminal(_)
            | Error::UnexpectedExit(_)
            | Error::GuestShutdown
            | Error::EventLoop(_) => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
