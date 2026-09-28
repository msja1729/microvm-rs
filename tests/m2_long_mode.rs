//! M2 acceptance: a 64-bit raw guest prints a greeting from long mode (AC-2.2), and the
//! vCPU really was in long mode when it halted (AC-2.3).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kvm_hello::{run_raw, RawConfig, RawMode};

const TIMEOUT: Duration = Duration::from_secs(10);
const GREETING: &str = "Hello from long mode!\n";
const EFER_LMA: u64 = 1 << 10;

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

/// A `Write` sink the test can read back after the VMM has taken ownership of it.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn long_mode_guest_prints_greeting_and_exits_zero() {
    let image = guest_image("hello64.bin");
    let (status, stdout) = run_vmm(&["raw", "--image", image.to_str().unwrap(), "--mode", "long"]);
    assert_eq!(String::from_utf8_lossy(&stdout), GREETING);
    assert_eq!(status.code(), Some(0));
}

#[test]
fn vcpu_is_in_long_mode_after_halt() {
    let config = RawConfig {
        kvm_device: PathBuf::from("/dev/kvm"),
        image: guest_image("hello64.bin"),
        mode: RawMode::Long,
        mem_mib: 16,
    };
    let out = SharedBuf::default();

    // The guest halts right after printing, so this cannot hang the way a spawned
    // process could; a guest bug shows up as an Err (e.g. triple-fault shutdown).
    let outcome = run_raw(&config, out.clone()).expect("raw long-mode guest failed");

    assert_eq!(String::from_utf8_lossy(&out.0.lock().unwrap()), GREETING);
    assert_ne!(outcome.sregs.efer & EFER_LMA, 0, "EFER.LMA must be set");
    assert_eq!(outcome.sregs.cs.l, 1, "CS.L must be set");
}
