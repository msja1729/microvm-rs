//! M3 acceptance: boot Linux 6.12 + BusyBox initramfs to an interactive shell
//! (AC-3.3 – AC-3.6), driven through a pseudo-terminal with rexpect.
//!
//! Needs `artifacts/bzImage` and `artifacts/initramfs.cpio`, so every test is
//! `#[ignore]`d; run with `cargo test --test m3_linux_boot -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rexpect::session::{spawn_command, PtySession};

/// AC-3.3: process start to init-ready marker.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// AC-3.6: `reboot -f` to VMM exit.
const REBOOT_TIMEOUT: Duration = Duration::from_secs(10);

/// The tests each boot a VM. Running them one at a time keeps the timing criteria
/// meaningful on a slow nested-virtualization host, even under a parallel harness.
static ONE_VM_AT_A_TIME: Mutex<()> = Mutex::new(());

fn artifact(name: &str, script: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts")
        .join(name);
    assert!(
        path.exists(),
        "{} is missing: run scripts/{script} first",
        path.display()
    );
    path.to_str().expect("artifact path is UTF-8").to_owned()
}

fn kernel() -> String {
    artifact("bzImage", "build-kernel.sh")
}

fn initrd() -> String {
    artifact("initramfs.cpio", "build-initramfs.sh")
}

fn spawn(command: Command) -> PtySession {
    // The per-expectation timeout; each test also checks its own deadline.
    spawn_command(command, Some(BOOT_TIMEOUT.as_millis() as u64)).expect("failed to spawn")
}

fn spawn_vmm() -> PtySession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vmm"));
    command.args(["boot", "--kernel", &kernel(), "--initrd", &initrd()]);
    spawn(command)
}

fn wait_for_prompt(session: &mut PtySession) {
    session
        .exp_string("PHASE1-INIT-OK")
        .expect("init marker not printed");
    session
        .exp_string("phase1# ")
        .expect("shell prompt not printed");
}

#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn boot_prints_init_marker_within_30s() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let start = Instant::now();
    let mut session = spawn_vmm();
    session
        .exp_string("PHASE1-INIT-OK")
        .expect("init marker not printed");
    let elapsed = start.elapsed();
    assert!(elapsed <= BOOT_TIMEOUT, "boot took {elapsed:?}");
}

#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn shell_receives_keyboard_input() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = spawn_vmm();
    wait_for_prompt(&mut session);
    // The echoed command line contains `$((6*7))`, never `42`, so only the
    // shell's evaluated output can match.
    session.send_line("echo vmm-$((6*7))").expect("send failed");
    session
        .exp_string("vmm-42")
        .expect("command output missing");
}

#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn uname_reports_linux_6_12() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = spawn_vmm();
    wait_for_prompt(&mut session);
    session.send_line("uname -r").expect("send failed");
    // Anchored to a line start so the echoed `uname -r` cannot match.
    session
        .exp_regex(r"\n6\.12")
        .expect("kernel version is not 6.12");
}

#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn reboot_exits_zero_and_restores_terminal() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // rexpect starts the pty with ECHO off; turn it on so the check below proves
    // the VMM restored settings it had changed (raw mode clears ICANON and ECHO).
    let mut command = Command::new("sh");
    command.args([
        "-c",
        r#"stty echo; "$0" boot --kernel "$1" --initrd "$2"; echo "VMM-EXIT=$?"; stty -a"#,
        env!("CARGO_BIN_EXE_vmm"),
        &kernel(),
        &initrd(),
    ]);
    let mut session = spawn(command);
    wait_for_prompt(&mut session);

    session.send_line("reboot -f").expect("send failed");
    let start = Instant::now();
    let (_, exit) = session
        .exp_regex(r"VMM-EXIT=\d+")
        .expect("VMM did not exit");
    let elapsed = start.elapsed();
    assert_eq!(exit, "VMM-EXIT=0");
    assert!(elapsed <= REBOOT_TIMEOUT, "reboot took {elapsed:?}");

    let stty = session.exp_eof().expect("stty output missing");
    let flags: Vec<&str> = stty
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect();
    for flag in ["icanon", "echo"] {
        assert!(
            flags.contains(&flag),
            "terminal not restored: `{flag}` missing from:\n{stty}"
        );
    }
}
