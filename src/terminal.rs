//! Raw-mode handling for the host terminal, with a guaranteed restore.
//!
//! In raw mode every keystroke (including Ctrl-C) goes straight to the guest. The
//! original settings come back on every exit path: the guard's `Drop` covers normal
//! returns, `?` errors and unwinding panics; a signal handler covers SIGINT, SIGTERM
//! and SIGHUP sent to the VMM process.

use std::io::IsTerminal;
use std::sync::OnceLock;

use crate::error::{Error, Result};

/// Terminal settings from before the VMM changed them. Set once, then only read
/// (also from the signal handler, which is why this is a lock-free `OnceLock`).
static ORIGINAL: OnceLock<libc::termios> = OnceLock::new();

const SIGNAL_MESSAGE: &[u8] = b"vmm: terminated by signal\n";

/// Put back the saved terminal settings, if any. Async-signal-safe.
fn restore() {
    if let Some(original) = ORIGINAL.get() {
        // SAFETY: tcsetattr only reads the valid `termios` we pass by reference; fd 0
        // is stdin. A failure leaves the terminal as it is, which is all we can do.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original) };
    }
}

extern "C" fn on_terminating_signal(_signal: libc::c_int) {
    restore();
    // SAFETY: write(2) and _exit(2) are async-signal-safe; the buffer is a valid
    // static byte slice of the given length.
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            SIGNAL_MESSAGE.as_ptr().cast(),
            SIGNAL_MESSAGE.len(),
        );
        libc::_exit(1);
    }
}

/// Make SIGINT, SIGTERM and SIGHUP restore the terminal and exit with code 1.
pub fn install_signal_handlers() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `sigaction` is plain data, so all-zero is a valid starting value
        // (empty mask, no flags).
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_terminating_signal as extern "C" fn(libc::c_int) as usize;
        // SAFETY: `action` is fully initialized and points at an `extern "C"` handler
        // that only calls async-signal-safe functions; we don't need the old action.
        let ret = unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        if ret != 0 {
            return Err(Error::Terminal(format!(
                "sigaction({signal}) failed: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

/// Restores the terminal when dropped.
pub struct RawModeGuard(());

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// If stdin is a terminal, switch it to raw mode and return a guard that restores it.
/// Returns `None` (and changes nothing) when stdin is not a terminal.
pub fn enter_raw_mode() -> Result<Option<RawModeGuard>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    // SAFETY: `termios` is plain data; tcgetattr overwrites it entirely on success.
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: fd 0 is stdin and `original` is a valid, writable `termios`.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
        return Err(Error::Terminal(format!(
            "tcgetattr failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    // Only the first call saves settings; later calls would see raw mode already.
    let saved = *ORIGINAL.get_or_init(|| original);

    let mut raw = saved;
    // SAFETY: cfmakeraw only modifies the valid `termios` we pass.
    unsafe { libc::cfmakeraw(&mut raw) };
    // SAFETY: fd 0 is stdin and `raw` is a valid `termios`.
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        return Err(Error::Terminal(format!(
            "tcsetattr failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(Some(RawModeGuard(())))
}
