//! Attach to an existing host TAP interface (never creates one: that needs root).
//!
//! The TAP is opened with `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`: whole Ethernet frames,
//! no packet-information prefix, and a virtio-net header in front of every frame. The
//! header size is set to 12 bytes to match the virtio 1.x header the device uses, and
//! all offloads are turned off so the host never hands the guest GSO or partial
//! checksums it did not negotiate.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

use vmm_sys_util::ioctl::{ioctl_with_ref, ioctl_with_val};
use vmm_sys_util::ioctl_iow_nr;

use crate::error::{Error, Result};

/// ioctl type for TUN/TAP (`'T'`).
const TUNTAP: std::os::raw::c_uint = 0x54;
ioctl_iow_nr!(TUNSETIFF, TUNTAP, 202, std::os::raw::c_int);
ioctl_iow_nr!(TUNSETOFFLOAD, TUNTAP, 208, std::os::raw::c_uint);
ioctl_iow_nr!(TUNSETVNETHDRSZ, TUNTAP, 216, std::os::raw::c_int);

const IFF_TAP: i16 = 0x0002;
const IFF_NO_PI: i16 = 0x1000;
const IFF_VNET_HDR: i16 = 0x4000;
/// Interface names are at most 15 bytes plus a NUL.
const IFNAMSIZ: usize = 16;
/// Size of the virtio-net header the device and the TAP exchange (virtio 1.x).
pub const VNET_HDR_SIZE: i32 = 12;

const TUN_DEVICE: &str = "/dev/net/tun";

/// `struct ifreq` as `TUNSETIFF` uses it: the name, then `ifr_flags`, padded to the
/// kernel's 40-byte size (the union's largest member is 24 bytes).
#[repr(C)]
struct IfReq {
    name: [u8; IFNAMSIZ],
    flags: i16,
    _pad: [u8; 22],
}

/// An open TAP queue in non-blocking mode. One `read` returns one frame and one
/// `write` sends one frame, each starting with the 12-byte virtio-net header.
#[derive(Debug)]
pub struct Tap {
    file: File,
}

/// Attach to the existing TAP interface `name`.
pub fn open(name: &str) -> Result<Tap> {
    let tap_error = |reason: String| Error::Tap {
        name: name.to_owned(),
        reason,
    };
    if name.is_empty() || name.len() >= IFNAMSIZ || name.contains(['/', '\0']) {
        return Err(tap_error(format!(
            "not a valid interface name (1..={} bytes, no '/')",
            IFNAMSIZ - 1
        )));
    }
    // Without CAP_NET_ADMIN, TUNSETIFF on a missing name fails with EPERM, which would
    // read like a permission problem; check existence first.
    if !Path::new("/sys/class/net").join(name).exists() {
        return Err(tap_error(
            "no such network interface (create it with: sudo scripts/setup-tap.sh)".into(),
        ));
    }
    if !Path::new(TUN_DEVICE).exists() {
        return Err(tap_error(format!(
            "{TUN_DEVICE} is missing; this kernel (e.g. a custom WSL2 kernel) has no TUN/TAP support"
        )));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(TUN_DEVICE)
        .map_err(|e| tap_error(format!("cannot open {TUN_DEVICE}: {e}")))?;

    let mut req = IfReq {
        name: [0; IFNAMSIZ],
        flags: IFF_TAP | IFF_NO_PI | IFF_VNET_HDR,
        _pad: [0; 22],
    };
    req.name[..name.len()].copy_from_slice(name.as_bytes());
    // TUNSETIFF: attach this file descriptor to the existing TAP `name` with the flags
    // above. The kernel only allows it if the interface's owner is our uid (or we have
    // CAP_NET_ADMIN) and no other process is attached.
    // SAFETY: `file` is an open /dev/net/tun descriptor and `req` is a live, correctly
    // sized `struct ifreq` the kernel reads (and may write the name back into).
    let ret = unsafe { ioctl_with_ref(&file, TUNSETIFF(), &req) };
    if ret < 0 {
        let err = io::Error::last_os_error();
        let reason = match err.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EBUSY) => {
                format!("cannot attach ({err}): not owned by you or already in use")
            }
            _ => format!("TUNSETIFF failed: {err}"),
        };
        return Err(tap_error(reason));
    }

    // TUNSETVNETHDRSZ: the kernel's default header is 10 bytes (legacy); virtio 1.x
    // always uses 12 (with `num_buffers`).
    // SAFETY: `file` is an attached TAP descriptor; the kernel reads one `int` from the
    // valid reference.
    let ret = unsafe { ioctl_with_ref(&file, TUNSETVNETHDRSZ(), &VNET_HDR_SIZE) };
    if ret < 0 {
        return Err(tap_error(format!(
            "TUNSETVNETHDRSZ failed: {}",
            io::Error::last_os_error()
        )));
    }
    // TUNSETOFFLOAD: no checksum or segmentation offloads; the guest negotiates none.
    // SAFETY: `file` is an attached TAP descriptor; the argument is passed by value.
    let ret = unsafe { ioctl_with_val(&file, TUNSETOFFLOAD(), 0) };
    if ret < 0 {
        return Err(tap_error(format!(
            "TUNSETOFFLOAD failed: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(Tap { file })
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Read for Tap {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for Tap {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifreq_has_kernel_layout() {
        assert_eq!(std::mem::size_of::<IfReq>(), 40);
        assert_eq!(std::mem::offset_of!(IfReq, flags), 16);
    }

    #[test]
    fn ioctl_numbers_match_linux_headers() {
        assert_eq!(TUNSETIFF(), 0x4004_54ca);
        assert_eq!(TUNSETOFFLOAD(), 0x4004_54d0);
        assert_eq!(TUNSETVNETHDRSZ(), 0x4004_54d8);
    }

    #[test]
    fn rejects_bad_or_missing_names() {
        for name in ["", "a/b", "sixteen-chars-xx", "vmm-no-such-if0"] {
            let err = open(name).expect_err(name);
            assert_eq!(err.exit_code(), 2);
            assert!(err.to_string().contains(name), "{err}");
        }
    }
}
