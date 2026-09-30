//! Per-thread seccomp-BPF allowlists. Every long-lived thread installs its filter
//! before it handles guest-controlled data; any syscall not listed kills the whole
//! process (`SECCOMP_RET_KILL_PROCESS`, i.e. `SIGSYS`).
//!
//! The lists were derived from `strace -f -ff` traces of `vmm boot` running the M4-M6
//! workloads (disk I/O and flush, ping, HTTP download, 4 vCPUs, `reboot -f`) and a
//! SIGTERM exit; see notes/phase-2.md. A signal handler can run on any thread, so every
//! filter allows what the terminal-restoring handler uses: `ioctl(TCGETS/TCSETS)`
//! (glibc's `tcsetattr`), `write` and `exit_group`.

use std::collections::BTreeMap;

use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};

use crate::error::{Error, Result};

/// `KVM_RUN`: `_IO(KVMIO, 0x80)`.
const KVM_RUN: u64 = 0xae80;

/// The kinds of long-lived threads, each with its own allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadKind {
    /// The main thread once it runs the event loop.
    Main,
    /// A vCPU thread.
    Vcpu,
    /// The thread copying host stdin into the serial device.
    Stdin,
}

fn seccomp_error(e: impl std::fmt::Display) -> Error {
    Error::Seccomp(e.to_string())
}

/// A rule matching when syscall argument `arg` equals `value`.
fn arg_eq(arg: u8, value: u64) -> Result<SeccompRule> {
    let cond = SeccompCondition::new(arg, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, value)
        .map_err(seccomp_error)?;
    SeccompRule::new(vec![cond]).map_err(seccomp_error)
}

/// Rules allowing `mmap`/`mprotect` only when `prot` (argument 2) lacks `PROT_EXEC`, so
/// no thread can ever make memory executable.
fn no_exec() -> Result<Vec<SeccompRule>> {
    let exec = libc::PROT_EXEC as u64;
    let cond = SeccompCondition::new(2, SeccompCmpArgLen::Dword, SeccompCmpOp::MaskedEq(exec), 0)
        .map_err(seccomp_error)?;
    Ok(vec![SeccompRule::new(vec![cond]).map_err(seccomp_error)?])
}

/// The `ioctl` requests `kind` may make.
fn ioctl_rules(kind: ThreadKind) -> Result<Vec<SeccompRule>> {
    let mut requests = vec![libc::TCGETS, libc::TCSETS];
    if kind == ThreadKind::Vcpu {
        requests.push(KVM_RUN);
    }
    requests.into_iter().map(|r| arg_eq(1, r)).collect()
}

fn rules(kind: ThreadKind) -> Result<BTreeMap<i64, Vec<SeccompRule>>> {
    // Every thread.
    let mut list: Vec<(i64, Vec<SeccompRule>)> = vec![
        (libc::SYS_ioctl, ioctl_rules(kind)?), // TCGETS/TCSETS: tcsetattr in the signal handler (and the exit-time restore); KVM_RUN on vCPUs
        (libc::SYS_write, vec![]), // serial output, logs, eventfd signals, TAP frames, the signal handler's message
        (libc::SYS_exit_group, vec![]), // the signal handler's _exit(1); normal process exit
        (libc::SYS_rt_sigreturn, vec![]), // return from a signal handler (the vCPU kick)
        (libc::SYS_futex, vec![]), // Mutex waits/wake-ups between threads; thread join
        (libc::SYS_rt_sigprocmask, vec![]), // glibc blocks signals around pthread_kill and thread exit
        (libc::SYS_sigaltstack, vec![]), // std removes a thread's signal stack when the thread ends
        (libc::SYS_mmap, no_exec()?), // allocator: large blocks and new heap regions (never executable)
        (libc::SYS_mprotect, no_exec()?), // allocator: growing a thread's heap arena (never executable)
        (libc::SYS_munmap, vec![]), // allocator frees; thread stacks, kvm_run and guest RAM at teardown
        (libc::SYS_madvise, vec![]), // allocator returns freed pages; a finished thread drops its stack
        (libc::SYS_brk, vec![]),     // allocator: growing the main heap
        (libc::SYS_mremap, vec![]),  // allocator: realloc of large (mmap-backed) blocks
    ];
    match kind {
        ThreadKind::Main => list.extend([
            (libc::SYS_epoll_wait, vec![]), // the event loop waits for eventfds and the TAP
            (libc::SYS_epoll_ctl, vec![]),  // start/stop watching the TAP (RX flow control)
            (libc::SYS_read, vec![]),       // drain eventfds; read frames from the TAP
            (libc::SYS_pread64, vec![]), // virtio-blk IN: FileExt::read_exact_at on the disk image
            (libc::SYS_pwrite64, vec![]), // virtio-blk OUT: FileExt::write_all_at on the disk image
            (libc::SYS_fdatasync, vec![]), // virtio-blk FLUSH: File::sync_data
            (libc::SYS_getpid, vec![]),  // pthread_kill (vCPU kick) looks up the process id
            (libc::SYS_tgkill, vec![]),  // pthread_kill: send SIGRTMIN to a vCPU thread
            (libc::SYS_clock_nanosleep, vec![]), // 10 ms pause between vCPU kicks
            (libc::SYS_fcntl, vec![]),   // F_GETFD: std's debug-build check before closing a file
            (libc::SYS_close, vec![]),   // closing files, eventfds, the TAP and KVM fds at exit
        ]),
        ThreadKind::Vcpu => list.extend([
            (libc::SYS_fcntl, vec![]), // F_GETFD: std's debug-build check before closing the vCPU fd
            (libc::SYS_close, vec![]), // dropping the vCPU fd when the thread ends
            (libc::SYS_exit, vec![]),  // the thread ends
        ]),
        ThreadKind::Stdin => list.extend([
            (libc::SYS_read, vec![]),            // read host stdin
            (libc::SYS_clock_nanosleep, vec![]), // wait 1 ms when the guest's RX FIFO is full
            (libc::SYS_exit, vec![]),            // the thread ends on stdin EOF or error
        ]),
    }
    Ok(list.into_iter().collect())
}

/// Compile the allowlist for `kind` into a BPF program.
pub fn filter(kind: ThreadKind) -> Result<BpfProgram> {
    SeccompFilter::new(
        rules(kind)?,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        TargetArch::x86_64,
    )
    .map_err(seccomp_error)?
    .try_into()
    .map_err(seccomp_error)
}

/// All three filters, compiled once before any thread starts.
#[derive(Debug, Clone)]
pub struct Filters {
    pub main: BpfProgram,
    pub vcpu: BpfProgram,
    pub stdin: BpfProgram,
}

impl Filters {
    pub fn compile() -> Result<Self> {
        Ok(Self {
            main: filter(ThreadKind::Main)?,
            vcpu: filter(ThreadKind::Vcpu)?,
            stdin: filter(ThreadKind::Stdin)?,
        })
    }
}

/// `PR_SET_NO_NEW_PRIVS`: this thread and every thread created later can never gain
/// privileges (e.g. through a setuid exec). Required before installing a filter
/// without `CAP_SYS_ADMIN`. Called once, before any thread is spawned.
pub fn set_no_new_privs() -> Result<()> {
    // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) takes no pointers and only
    // changes this process's privilege flags.
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(Error::Seccomp(format!(
            "prctl(PR_SET_NO_NEW_PRIVS) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Install `program` on the calling thread. From now on any other syscall on this
/// thread kills the process.
pub fn install(program: &BpfProgram) -> Result<()> {
    apply_filter(program).map_err(seccomp_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_filter_compiles_to_bpf() {
        for kind in [ThreadKind::Main, ThreadKind::Vcpu, ThreadKind::Stdin] {
            let program = filter(kind).expect("filter compiles");
            assert!(!program.is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn only_vcpus_may_run_kvm() {
        for (kind, rules) in [
            (ThreadKind::Main, 2),
            (ThreadKind::Stdin, 2),
            (ThreadKind::Vcpu, 3),
        ] {
            assert_eq!(ioctl_rules(kind).expect("rules").len(), rules, "{kind:?}");
        }
    }

    #[test]
    fn no_thread_may_open_sockets_or_files() {
        for kind in [ThreadKind::Main, ThreadKind::Vcpu, ThreadKind::Stdin] {
            let rules = rules(kind).expect("rules");
            for sc in [
                libc::SYS_socket,
                libc::SYS_openat,
                libc::SYS_execve,
                libc::SYS_clone3,
            ] {
                assert!(!rules.contains_key(&sc), "{kind:?} allows syscall {sc}");
            }
        }
    }
}
