//! M6 acceptance: multiple vCPUs (AC-6.1 – AC-6.5).
//!
//! Tests that boot need the built artifacts (and AC-6.4 the `vmtap0` TAP), so they are
//! `#[ignore]`d; run with `cargo test --test m6_smp -- --ignored --test-threads=1`.
//! `ac_6_1` always runs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rexpect::session::{spawn_command, PtySession};

/// Spec performance bound: disk + net + 4 vCPUs reach the init marker in 30 s.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// AC-6.5: `reboot -f` to VMM exit.
const REBOOT_TIMEOUT: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(60);
const SKIP: &str = "SKIP (run: sudo scripts/setup-tap.sh)";

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

fn require_tap() {
    let owner = std::fs::read_to_string("/sys/class/net/vmtap0/owner").unwrap_or_default();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    assert_eq!(
        owner.trim(),
        uid.to_string(),
        "vmtap0 missing or not ours: {SKIP}"
    );
    let addr = Command::new("ip")
        .args(["-4", "addr", "show", "vmtap0"])
        .output()
        .expect("run ip");
    assert!(
        String::from_utf8_lossy(&addr.stdout).contains("172.16.0.1/24"),
        "vmtap0 has no 172.16.0.1/24: {SKIP}"
    );
}

/// Boot with `extra` arguments inside `sh`, which prints `VMM-EXIT=<code>` when the
/// VMM exits. Returns the session at the shell prompt and the time to the init marker.
fn boot(extra: &[&str]) -> (PtySession, Duration) {
    let kernel = artifact("bzImage", "build-kernel.sh");
    let initrd = artifact("initramfs.cpio", "build-initramfs.sh");
    let mut command = Command::new("sh");
    command.args([
        "-c",
        r#"vmm=$1; shift; "$vmm" boot "$@"; echo "VMM-EXIT=$?""#,
        "sh",
        env!("CARGO_BIN_EXE_vmm"),
        "--kernel",
        s(&kernel),
        "--initrd",
        s(&initrd),
    ]);
    command.args(extra);
    let start = Instant::now();
    let mut session =
        spawn_command(command, Some(TIMEOUT.as_millis() as u64)).expect("failed to spawn");
    session
        .exp_string("PHASE1-INIT-OK")
        .expect("init marker not printed");
    let boot_time = start.elapsed();
    session
        .exp_string("phase1# ")
        .expect("shell prompt not printed");
    (session, boot_time)
}

fn run(session: &mut PtySession, cmd: &str) -> String {
    session.send_line(cmd).expect("send failed");
    session
        .exp_string("phase1# ")
        .unwrap_or_else(|e| panic!("no prompt after `{cmd}`: {e}"))
}

/// `reboot -f`; asserts exit code 0 and returns how long the VMM took to exit.
fn reboot(mut session: PtySession) -> Duration {
    session.send_line("reboot -f").expect("send failed");
    let start = Instant::now();
    let (_, exit) = session
        .exp_regex(r"VMM-EXIT=\d+")
        .expect("VMM did not exit");
    let elapsed = start.elapsed();
    assert_eq!(exit, "VMM-EXIT=0");
    elapsed
}

#[test]
fn ac_6_1_vcpu_count_out_of_range_exit_2() {
    for n in ["0", "5"] {
        let out = Command::new(env!("CARGO_BIN_EXE_vmm"))
            .args(["boot", "--kernel", "/nonexistent/bzImage", "--vcpus", n])
            .output()
            .expect("run vmm");
        assert_eq!(out.status.code(), Some(2), "--vcpus {n}: {out:?}");
    }
}

#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn ac_6_2_guest_sees_four_cpus() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (mut session, _) = boot(&["--vcpus", "4"]);
    // `echo` markers keep the command echo (which contains no bare "4") from matching.
    let out = run(&mut session, "echo nproc=$(nproc)");
    assert!(out.contains("\nnproc=4"), "{out}");
    let out = run(
        &mut session,
        "echo cpuinfo=$(grep -c ^processor /proc/cpuinfo)",
    );
    assert!(out.contains("\ncpuinfo=4"), "{out}");
    reboot(session);
}

#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn ac_6_3_every_cpu_takes_timer_interrupts() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (mut session, _) = boot(&["--vcpus", "4"]);
    let out = run(&mut session, "grep LOC: /proc/interrupts");
    let row = out
        .lines()
        .find(|l| l.trim_start().starts_with("LOC:"))
        .unwrap_or_else(|| panic!("no LOC row:\n{out}"));
    let counts: Vec<u64> = row
        .split_whitespace()
        .skip(1)
        .take(4)
        .map(|c| c.parse().expect("numeric count"))
        .collect();
    assert_eq!(counts.len(), 4, "{row}");
    assert!(
        counts.iter().all(|&c| c > 0),
        "a CPU has no timer interrupts: {row}"
    );
    reboot(session);
}

#[test]
#[ignore = "needs artifacts, artifacts/disk.ext4 and the vmtap0 TAP"]
fn ac_6_4_ten_boots_with_disk_net_and_four_vcpus() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    require_tap();
    let dir = tempfile::tempdir().expect("tempdir");
    let disk = dir.path().join("disk.ext4");
    std::fs::copy(artifact("disk.ext4", "build-disk.sh"), &disk).expect("copy disk");
    for i in 1..=10 {
        let (session, boot_time) = boot(&["--vcpus", "4", "--disk", s(&disk), "--tap", "vmtap0"]);
        assert!(boot_time <= BOOT_TIMEOUT, "boot {i} took {boot_time:?}");
        reboot(session);
        eprintln!("boot {i}: init marker after {boot_time:?}");
    }
}

#[test]
#[ignore = "needs artifacts/bzImage and artifacts/initramfs.cpio"]
fn ac_6_5_reboot_with_four_vcpus_exits_within_10s() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (session, _) = boot(&["--vcpus", "4"]);
    let elapsed = reboot(session);
    assert!(elapsed <= REBOOT_TIMEOUT, "reboot took {elapsed:?}");
}
