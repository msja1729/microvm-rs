//! M4 acceptance: virtio-blk (AC-4.3 – AC-4.8), driven through a pseudo-terminal with
//! rexpect. Every test works on a temporary copy of `artifacts/disk.ext4`.
//!
//! Set `VMM_TEST_VCPUS=<n>` to boot with `--vcpus <n>`.
//! `ac_4_8` needs no artifacts and always runs. Tests that boot need `artifacts/bzImage`, `artifacts/initramfs.cpio` and
//! `artifacts/disk.ext4`, so they are `#[ignore]`d; run with
//! `cargo test --test m4_virtio_blk -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use rexpect::session::{spawn_command, PtySession};
use tempfile::TempDir;

/// Per-expectation timeout. Generous: writing 32 MiB of random data under nested
/// virtualization takes a while.
const TIMEOUT: Duration = Duration::from_secs(120);

/// One VM at a time keeps timings meaningful on a slow nested-virtualization host.
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

/// `--vcpus $VMM_TEST_VCPUS` when the variable is set, so the verify script can rerun
/// these tests with 4 vCPUs (AC-6.6, AC-7.3).
fn vcpu_args() -> Vec<String> {
    match std::env::var("VMM_TEST_VCPUS") {
        Ok(n) => vec!["--vcpus".into(), n],
        Err(_) => Vec::new(),
    }
}

/// A private copy of the disk image, deleted with the returned directory.
fn disk_copy() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let copy = dir.path().join("disk.ext4");
    std::fs::copy(artifact("disk.ext4", "build-disk.sh"), &copy).expect("copy disk image");
    (dir, copy)
}

/// Boot the VMM with `extra` arguments inside `sh`, which prints `VMM-EXIT=<code>`
/// when the VMM exits.
fn boot(extra: &[&str]) -> PtySession {
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
    command.args(vcpu_args());
    command.args(extra);
    let mut session =
        spawn_command(command, Some(TIMEOUT.as_millis() as u64)).expect("failed to spawn");
    session
        .exp_string("PHASE1-INIT-OK")
        .expect("init marker not printed");
    session
        .exp_string("phase1# ")
        .expect("shell prompt not printed");
    session
}

/// Run `cmd` and wait for the next prompt; returns everything printed in between.
fn run(session: &mut PtySession, cmd: &str) -> String {
    session.send_line(cmd).expect("send failed");
    session
        .exp_string("phase1# ")
        .unwrap_or_else(|e| panic!("no prompt after `{cmd}`: {e}"))
}

/// `reboot -f` and assert the VMM exits with code 0.
fn reboot(mut session: PtySession) {
    session.send_line("reboot -f").expect("send failed");
    let (_, exit) = session
        .exp_regex(r"VMM-EXIT=\d+")
        .expect("VMM did not exit");
    assert_eq!(exit, "VMM-EXIT=0");
}

fn mount(session: &mut PtySession) {
    let out = run(
        session,
        "mkdir -p /mnt && mount /dev/vda /mnt && echo MOUNT-OK",
    );
    assert!(out.contains("\nMOUNT-OK"), "mount failed:\n{out}");
}

fn debugfs(request: &str, image: &Path) -> String {
    let out = Command::new("debugfs")
        .args(["-R", request, s(image)])
        .output()
        .expect("run debugfs (apt install e2fsprogs)");
    assert!(out.status.success(), "debugfs failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sha256(path: &Path) -> String {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("run sha256sum");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("sha256sum output")
        .to_owned()
}

#[test]
#[ignore = "needs artifacts/bzImage, initramfs.cpio and disk.ext4"]
fn ac_4_3_disk_capacity_is_64_mib() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, disk) = disk_copy();
    let mut session = boot(&["--disk", s(&disk)]);
    let out = run(&mut session, "cat /sys/block/vda/size");
    assert!(out.contains("\n131072"), "unexpected size:\n{out}");
    reboot(session);
}

#[test]
#[ignore = "needs artifacts/bzImage, initramfs.cpio and disk.ext4"]
fn ac_4_4_mount_and_read_hello() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, disk) = disk_copy();
    let mut session = boot(&["--disk", s(&disk)]);
    mount(&mut session);
    let out = run(&mut session, "cat /mnt/hello.txt");
    assert!(out.contains("\nphase2-disk"), "unexpected contents:\n{out}");
    reboot(session);
}

#[test]
#[ignore = "needs artifacts/bzImage, initramfs.cpio and disk.ext4"]
fn ac_4_5_writes_persist_across_vmm_processes() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, disk) = disk_copy();

    let mut first = boot(&["--disk", s(&disk)]);
    mount(&mut first);
    run(
        &mut first,
        "echo persist-ok > /mnt/p.txt && sync && umount /mnt",
    );
    reboot(first);

    let mut second = boot(&["--disk", s(&disk)]);
    mount(&mut second);
    let out = run(&mut second, "cat /mnt/p.txt");
    assert!(out.contains("\npersist-ok"), "not persisted:\n{out}");
    run(&mut second, "umount /mnt");
    reboot(second);

    assert!(debugfs("cat /p.txt", &disk).contains("persist-ok"));
}

#[test]
#[ignore = "needs artifacts/bzImage, initramfs.cpio and disk.ext4"]
fn ac_4_6_32_mib_file_integrity() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, disk) = disk_copy();
    let mut session = boot(&["--disk", s(&disk)]);
    mount(&mut session);
    run(
        &mut session,
        "dd if=/dev/urandom of=/mnt/big bs=1M count=32 2>/dev/null",
    );
    session
        .send_line("sha256sum /mnt/big")
        .expect("send failed");
    let (_, line) = session
        .exp_regex(r"[0-9a-f]{64}  /mnt/big")
        .expect("no checksum from the guest");
    let guest_sum = line[..64].to_owned();
    session.exp_string("phase1# ").expect("no prompt");
    run(&mut session, "umount /mnt");
    reboot(session);

    let extracted = dir.path().join("big");
    debugfs(&format!("dump /big {}", s(&extracted)), &disk);
    assert_eq!(
        std::fs::metadata(&extracted).expect("dumped file").len(),
        32 << 20
    );
    assert_eq!(sha256(&extracted), guest_sum);
}

#[test]
#[ignore = "needs artifacts/bzImage, initramfs.cpio and disk.ext4"]
fn ac_4_7_read_only_disk() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, disk) = disk_copy();
    let before = sha256(&disk);
    let mut session = boot(&["--disk", s(&disk), "--disk-ro"]);
    run(&mut session, "mkdir -p /mnt; mount /dev/vda /mnt");
    let out = run(&mut session, "echo x > /mnt/y");
    assert!(
        out.contains("Read-only file system"),
        "write did not fail:\n{out}"
    );
    let out = run(&mut session, "cat /mnt/hello.txt");
    assert!(out.contains("\nphase2-disk"), "read failed:\n{out}");
    reboot(session);
    assert_eq!(sha256(&disk), before, "read-only image changed");
}

/// Run the VMM with `args` (no kernel is needed: disk checks come first) and return
/// its exit code and stderr.
fn vmm_error(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_vmm"))
        .args(["boot", "--kernel", "/nonexistent/bzImage"])
        .args(args)
        .output()
        .expect("run vmm");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn ac_4_8_bad_disk_arguments_exit_2() {
    let dir = tempfile::tempdir().expect("tempdir");

    let missing = dir.path().join("missing.img");
    let (code, err) = vmm_error(&["--disk", s(&missing)]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains(s(&missing)), "{err}");

    for (name, size) in [("empty.img", 0), ("odd.img", 1000)] {
        let path = dir.path().join(name);
        std::fs::File::create(&path)
            .and_then(|f| f.set_len(size))
            .expect("create image");
        let (code, err) = vmm_error(&["--disk", s(&path)]);
        assert_eq!(code, Some(2), "{name}: {err}");
        assert!(err.contains(s(&path)), "{name}: {err}");
    }

    let (code, err) = vmm_error(&["--disk-ro"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--disk"), "{err}");
}
