//! M7 acceptance: seccomp hardening (AC-7.1, 7.2, 7.4, 7.5 at run time).
//!
//! Every test boots a VM, so all are `#[ignore]`d; run with
//! `cargo test --test m7_seccomp -- --ignored --test-threads=1`.

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rexpect::session::{spawn_command, PtySession};

const TIMEOUT: Duration = Duration::from_secs(60);

static ONE_VM_AT_A_TIME: Mutex<()> = Mutex::new(());

fn artifact(name: &str, script: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts")
        .join(name);
    assert!(
        path.exists(),
        "{} is missing: run scripts/{script} first",
        path.display()
    );
    path
}

fn s(path: &Path) -> &str {
    path.to_str().expect("path is UTF-8")
}

/// Boot through `sh`, which prints the VMM's pid (`exec` keeps it) before starting it.
fn boot(extra: &[&str]) -> (PtySession, u32) {
    let kernel = artifact("bzImage", "build-kernel.sh");
    let initrd = artifact("initramfs.cpio", "build-initramfs.sh");
    let mut command = Command::new("sh");
    command.args([
        "-c",
        r#"echo "VMM-PID=$$"; vmm=$1; shift; exec "$vmm" boot "$@""#,
        "sh",
        env!("CARGO_BIN_EXE_vmm"),
        "--kernel",
        s(&kernel),
        "--initrd",
        s(&initrd),
    ]);
    command.args(extra);
    let mut session =
        spawn_command(command, Some(TIMEOUT.as_millis() as u64)).expect("failed to spawn");
    let (_, pid) = session.exp_regex(r"VMM-PID=\d+").expect("no pid");
    let pid = pid["VMM-PID=".len()..].parse().expect("numeric pid");
    session
        .exp_string("PHASE1-INIT-OK")
        .expect("init marker not printed");
    session
        .exp_string("phase1# ")
        .expect("shell prompt not printed");
    (session, pid)
}

/// `(thread name, Seccomp mode, NoNewPrivs)` for every thread of process `pid`.
fn threads(pid: u32) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).expect("read task dir") {
        let status = std::fs::read_to_string(task.expect("task").path().join("status"))
            .expect("read status");
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .map(|v| v.trim().to_owned())
                .unwrap_or_default()
        };
        out.push((field("Name:"), field("Seccomp:"), field("NoNewPrivs:")));
    }
    out.sort();
    out
}

/// AC-7.1 / AC-7.2 at run time: every thread of a running VMM (main, stdin, each vCPU)
/// has a seccomp filter (mode 2) and no-new-privileges set.
#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn ac_7_1_every_thread_is_filtered() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (mut session, pid) = boot(&["--vcpus", "4"]);
    let threads = threads(pid);
    let names: BTreeSet<&str> = threads.iter().map(|(n, _, _)| n.as_str()).collect();
    let expected: BTreeSet<&str> = ["vmm", "stdin", "vcpu0", "vcpu1", "vcpu2", "vcpu3"].into();
    // KVM itself may add kernel worker threads to the process (e.g.
    // `kvm-nx-lpage-recovery`); they are not VMM threads, but they are checked too.
    let extra: Vec<&&str> = names.difference(&expected).collect();
    assert!(
        names.is_superset(&expected) && extra.iter().all(|n| n.starts_with("kvm-")),
        "unexpected thread set: {threads:?}"
    );
    for (name, seccomp, nnp) in &threads {
        assert_eq!(seccomp, "2", "thread {name} has no seccomp filter");
        assert_eq!(nnp, "1", "thread {name} lacks NoNewPrivs");
    }
    session.send_line("reboot -f").expect("send");
    session.exp_eof().expect("VMM exits");
}

/// AC-7.4: a vCPU thread calling a syscall outside its allowlist kills the whole VMM
/// with SIGSYS.
#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn ac_7_4_forbidden_syscall_kills_vmm_with_sigsys() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut child = Command::new(env!("CARGO_BIN_EXE_vmm"))
        .args([
            "boot",
            "--kernel",
            s(&artifact("bzImage", "build-kernel.sh")),
            "--initrd",
            s(&artifact("initramfs.cpio", "build-initramfs.sh")),
            "--seccomp-test-violation",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vmm");
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("VMM was not killed");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.signal(), Some(libc::SIGSYS), "{status:?}");
}

/// AC-7.5: `--no-seccomp` warns on stderr and still boots.
#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn ac_7_5_no_seccomp_warns_and_boots() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let kernel = artifact("bzImage", "build-kernel.sh");
    let initrd = artifact("initramfs.cpio", "build-initramfs.sh");
    // Only stderr reaches the pty, so the warning can't come from anywhere else.
    let mut command = Command::new("sh");
    command.args([
        "-c",
        r#""$0" boot --kernel "$1" --initrd "$2" --no-seccomp </dev/null >/dev/null"#,
        env!("CARGO_BIN_EXE_vmm"),
        s(&kernel),
        s(&initrd),
    ]);
    let mut session =
        spawn_command(command, Some(TIMEOUT.as_millis() as u64)).expect("failed to spawn");
    session
        .exp_string("vmm: warning: seccomp disabled (--no-seccomp); for debugging only")
        .expect("warning not on stderr");
    drop(session);

    let (mut session, pid) = boot(&["--no-seccomp"]);
    assert!(
        threads(pid).iter().all(|(_, seccomp, _)| seccomp == "0"),
        "a filter was installed despite --no-seccomp"
    );
    session.send_line("reboot -f").expect("send");
    session.exp_eof().expect("VMM exits");
}
