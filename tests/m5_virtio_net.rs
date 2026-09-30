//! M5 acceptance: virtio-net over the host TAP `vmtap0` (AC-5.2 – AC-5.6).
//!
//! Tests that boot need the built artifacts and the TAP precondition (`vmtap0` exists,
//! is owned by the current user and has 172.16.0.1/24; create it with
//! `sudo scripts/setup-tap.sh`), so they are `#[ignore]`d. Only one VMM can attach to
//! the TAP at a time: run them with `--test-threads=1`. `ac_5_6` always runs.

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rexpect::session::{spawn_command, PtySession};

const TIMEOUT: Duration = Duration::from_secs(120);
const TAP: &str = "vmtap0";
const HOST_IP: &str = "172.16.0.1";
const GUEST_IP: &str = "172.16.0.2";
const SKIP: &str = "SKIP (run: sudo scripts/setup-tap.sh)";

/// One VM at a time: they share the TAP.
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

/// The TAP precondition from the spec; fails the test with the SKIP message otherwise.
fn require_tap() {
    let owner = std::fs::read_to_string(format!("/sys/class/net/{TAP}/owner")).unwrap_or_default();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    assert_eq!(
        owner.trim(),
        uid.to_string(),
        "{TAP} missing or not owned by uid {uid}: {SKIP}"
    );
    let addr = Command::new("ip")
        .args(["-4", "addr", "show", TAP])
        .output()
        .expect("run ip");
    assert!(
        String::from_utf8_lossy(&addr.stdout).contains(&format!("{HOST_IP}/24")),
        "{TAP} has no {HOST_IP}/24: {SKIP}"
    );
}

/// Boot with the TAP attached (plus `extra` arguments) inside `sh`, which prints
/// `VMM-EXIT=<code>` when the VMM exits.
fn boot(extra: &[&str]) -> PtySession {
    require_tap();
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
        "--tap",
        TAP,
    ]);
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

fn run(session: &mut PtySession, cmd: &str) -> String {
    session.send_line(cmd).expect("send failed");
    session
        .exp_string("phase1# ")
        .unwrap_or_else(|e| panic!("no prompt after `{cmd}`: {e}"))
}

fn reboot(mut session: PtySession) {
    session.send_line("reboot -f").expect("send failed");
    let (_, exit) = session
        .exp_regex(r"VMM-EXIT=\d+")
        .expect("VMM did not exit");
    assert_eq!(exit, "VMM-EXIT=0");
}

fn configure_guest(session: &mut PtySession) {
    let out = run(
        session,
        &format!("ip addr add {GUEST_IP}/24 dev eth0 && ip link set eth0 up && echo NET-UP"),
    );
    assert!(
        out.contains("\nNET-UP"),
        "guest network setup failed:\n{out}"
    );
}

/// `, 0% packet loss` (the comma keeps `100% packet loss` from matching).
fn no_loss(ping_output: &str) -> bool {
    ping_output.contains(", 0% packet loss")
}

#[test]
#[ignore = "needs artifacts and the vmtap0 TAP"]
fn ac_5_2_guest_sees_default_mac() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = boot(&[]);
    let out = run(&mut session, "ip link show eth0");
    assert!(
        out.contains("link/ether 06:00:ac:10:00:02"),
        "wrong MAC:\n{out}"
    );
    reboot(session);
}

#[test]
#[ignore = "needs artifacts and the vmtap0 TAP"]
fn ac_5_3_guest_pings_host() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = boot(&[]);
    configure_guest(&mut session);
    let out = run(&mut session, &format!("ping -c 3 {HOST_IP}"));
    assert!(no_loss(&out), "guest -> host ping lost packets:\n{out}");
    reboot(session);
}

#[test]
#[ignore = "needs artifacts and the vmtap0 TAP"]
fn ac_5_4_host_pings_guest() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = boot(&[]);
    configure_guest(&mut session);
    let out = Command::new("ping")
        .args(["-c", "3", "-W", "5", GUEST_IP])
        .output()
        .expect("run host ping");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && no_loss(&text),
        "host -> guest ping failed:\n{text}"
    );
    reboot(session);
}

/// `python3 -m http.server`, killed when dropped.
struct HttpServer(Child);

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "needs artifacts and the vmtap0 TAP"]
fn ac_5_5_bulk_download_matches_host_checksum() {
    let _one = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    require_tap();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f");
    let mut data = vec![0u8; 16 << 20];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut r| r.read_exact(&mut data))
        .expect("random data");
    std::fs::write(&file, &data).expect("write file");
    let host_sum = String::from_utf8_lossy(
        &Command::new("sha256sum")
            .arg(&file)
            .output()
            .expect("sha256sum")
            .stdout,
    )[..64]
        .to_owned();

    let _server = HttpServer(
        Command::new("python3")
            .args([
                "-m",
                "http.server",
                "8000",
                "--bind",
                HOST_IP,
                "--directory",
            ])
            .arg(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start python3 http.server"),
    );
    let addr: SocketAddr = format!("{HOST_IP}:8000").parse().expect("address");
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
        assert!(Instant::now() < deadline, "http.server did not start");
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut session = boot(&[]);
    configure_guest(&mut session);
    session
        .send_line(&format!(
            "wget -q -O /f http://{HOST_IP}:8000/f && sha256sum /f"
        ))
        .expect("send failed");
    let (_, line) = session
        .exp_regex(r"[0-9a-f]{64}  /f")
        .expect("download or checksum failed");
    assert_eq!(&line[..64], host_sum);
    session.exp_string("phase1# ").expect("no prompt");
    reboot(session);
}

/// Run the VMM with `args` (argument errors come before the kernel is read) and return
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
fn ac_5_6_bad_net_arguments_exit_2() {
    let (code, err) = vmm_error(&["--tap", "vmm-notap0"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("vmm-notap0"), "{err}");

    let (code, err) = vmm_error(&["--guest-mac", "06:00:ac:10:00:03"]);
    assert_eq!(code, Some(2), "--guest-mac without --tap: {err}");

    for mac in ["06:00:ac:10:00", "zz:00:ac:10:00:02", "01:00:5e:00:00:01"] {
        let (code, err) = vmm_error(&["--tap", "vmm-notap0", "--guest-mac", mac]);
        assert_eq!(code, Some(2), "{mac}: {err}");
        assert!(err.contains(mac), "{mac}: {err}");
    }
}
