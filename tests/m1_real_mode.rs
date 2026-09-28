//! M1 acceptance: a 16-bit raw guest prints a greeting over COM1 and halts (AC-1.2, AC-1.3).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);

fn guest_image(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("guests/build")
        .join(name);
    assert!(
        path.exists(),
        "{} is missing: run scripts/build-guests.sh first",
        path.display()
    );
    path
}

/// Run the `vmm` binary with `args`, killing it after `TIMEOUT`. Returns (status, stdout).
fn run_vmm(args: &[&str]) -> (ExitStatus, Vec<u8>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vmm"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn vmm");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            break status;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("vmm did not exit within {TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = reader
        .join()
        .expect("reader thread panicked")
        .expect("reading stdout failed");
    (status, stdout)
}

#[test]
fn real_mode_guest_prints_greeting_and_exits_zero() {
    let image = guest_image("hello16.bin");
    let (status, stdout) = run_vmm(&["raw", "--image", image.to_str().unwrap(), "--mode", "real"]);
    assert_eq!(
        String::from_utf8_lossy(&stdout),
        "Hello from real mode!\n",
        "stdout must be exactly the greeting"
    );
    assert_eq!(status.code(), Some(0));
}
