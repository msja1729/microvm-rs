# Phase 1 — Decisions log

Choices made inside the spec during implementation. Format: question → options → choice → why.

## M1 — Real-mode hello

### D1. Crate versions
- **Options:** latest of each crate; or older versions to match a future `linux-loader`.
- **Chose:** latest compatible set — `kvm-ioctls 0.25`, `kvm-bindings 0.14`, `vm-memory 0.18` (`backend-mmap`), `vm-superio 0.8`, `clap 4`, `thiserror 2`, `log 0.4`, `env_logger 0.11`, `libc 0.2`. `cargo tree -d` shows no duplicated rust-vmm crates.
- **Why:** M1 needs none of the other crates. `linux-loader` is added in M3; if its `vm-memory` requirement differs, it is resolved then.

### D2. Library crate name
- **Options:** rename the lib to `vmm` (same as the binary); keep the package default `kvm_hello`.
- **Chose:** `kvm_hello`.
- **Why:** a lib and a bin with the same name make cargo's doc output collide. Only the binary name is specified.

### D3. `--mode long` during M1
- **Options:** accept `long` and fail at runtime; leave it out of the CLI until M2.
- **Chose:** leave it out. `--mode long` is currently rejected by clap (exit 2).
- **Why:** no scope creep; M2 adds the variant together with its implementation.

### D4. Port-I/O device interface
- **Options:** `vm-device` bus crate (not on the allowed list); a small trait of our own.
- **Chose:** `PioDevice` trait (`read(offset, &mut [u8])`, `write(offset, &[u8]) -> Result`) and a linear-scan `PioBus`.
- **Why:** only allowed crates may be used, and Phase 1 has two devices. `write` returns `Result` so a host stdout failure surfaces as exit 1 instead of being ignored.

### D5. Multi-byte access to the UART
- **Options:** use only the first byte; treat byte *i* as register `offset + i`.
- **Chose:** byte *i* goes to register `offset + i`; bytes past the 8-port range read `0xFF` / are ignored.
- **Why:** matches how x86 splits a wide `IN`/`OUT` across consecutive 8-bit ports, and cannot index out of range.

### D6. Where AC-0.4 is checked
- **Options:** a Rust integration test; the verify script.
- **Chose:** `scripts/verify-phase-1.sh` runs the real binary with `--kvm-device /nonexistent/kvm`.
- **Why:** the output-file list has no test file for AC-0.x, and the check is a single command.

### D7. `verify-phase-1.sh` contents during M1
- **Options:** write every criterion now with M2/M3 failing; add each milestone's checks when that milestone is implemented.
- **Chose:** AC-0.x and M1 checks only; M2 and M3 sections are added in their milestones.
- **Why:** each milestone's diff stays scoped to that milestone; the script still prints PASS/FAIL per criterion and exits non-zero on any failure.

### D8. Test timeout mechanism
- **Options:** a `wait-timeout` style crate (not allowed); poll `try_wait` with a reader thread.
- **Chose:** poll `try_wait` every 20 ms for up to 10 s, then kill and fail; stdout is drained on a separate thread so a full pipe can't deadlock the child.
- **Why:** only `rexpect` and `tempfile` are allowed dev-crates.

## M2 — Long-mode hello

### D9. When to set CPUID
- **Options:** only for `boot` (pitfall 3); for long mode too; for every vCPU.
- **Chose:** every vCPU gets `KVM_GET_SUPPORTED_CPUID` → `KVM_SET_CPUID2` right after creation.
- **Why:** KVM rejects `EFER.LME` unless the guest's CPUID advertises long mode, so M2 already needs it. It is harmless for real mode, and one code path is simpler than three.

### D10. Entering long mode
- **Options:** start in real mode and let a guest trampoline switch modes; set the final long-mode state directly with `KVM_SET_SREGS`.
- **Chose:** set it directly (CR0.PE|PG, CR4.PAE, EFER.LME|LMA, CR3, 64-bit CS) — the spec says "the VMM puts the vCPU into long mode".
- **Why:** it is what M3's 64-bit boot entry needs too, and it keeps guest code trivial.

### D11. Where GDT encoding is tested
- **Options:** only indirectly via the M2 integration test; unit tests too.
- **Chose:** two `#[cfg(test)]` unit tests inside `src/arch/x86_64/gdt.rs` (flat 64-bit code segment; base/limit round-trip).
- **Why:** a wrong bit gives an opaque triple fault; a unit test pinpoints it. It adds no file to the output list.

### D12. Shared test helpers
- **Options:** `tests/common/mod.rs`; duplicate the small `guest_image` / `run_vmm` helpers.
- **Chose:** duplicate them in `tests/m1_real_mode.rs` and `tests/m2_long_mode.rs`.
- **Why:** `tests/common/` is not in the spec's output-file list, and the helpers are ~40 lines.

### D13. How AC-2.3 reads the registers
- **Chose:** the in-process test calls `kvm_hello::run_raw`, which returns `RawOutcome { sregs }` after `HLT`, with serial output going to a test-owned `Arc<Mutex<Vec<u8>>>` writer. (Resolution of Q4.)

## M3 — Linux boot

### D14. Pinned kernel release
- **Chose:** Linux `6.12.111` (the newest 6.12.y on kernel.org at build time), SHA-256 `9e59dc67…7849810` from kernel.org's `sha256sums.asc`, checked by `build-kernel.sh`.

### D15. No `/dev/console` node in the initramfs
- **Options:** build the cpio with `fakeroot` (not installed) or the kernel tree's `gen_init_cpio`; hand-write a device node into the archive; have `/init` attach itself to the console.
- **Chose:** `/init` mounts devtmpfs first, then `exec 0</dev/console 1>/dev/console 2>&1`, then mounts proc and sysfs and prints the marker.
- **Why:** creating a device node needs root; the other options couple the initramfs build to the kernel build or add tools. The kernel runs `/init` even when it cannot open an initial console. All three mounts still happen before the marker, as the spec requires.

### D16. Sharing the UART between threads
- **Chose:** `Arc<Mutex<SerialDevice>>`; the vCPU thread locks it per port access (`SharedSerial`), the stdin thread per chunk of input. When the 64-byte RX FIFO is full the stdin thread sleeps 1 ms and retries. (Resolution of Q3.)
- **Why:** the guest drains the FIFO only from its interrupt handler, so input must wait for it rather than be dropped; 1 ms is far below human typing speed.

### D17. Restoring the terminal
- **Options:** Drop guard + panic hook + signal handler; Drop guard + signal handler.
- **Chose:** Drop guard (normal return, `?` errors, unwinding panics on the vCPU thread) plus a SIGINT/SIGTERM/SIGHUP handler that calls only async-signal-safe `tcsetattr`, `write` and `_exit`. The saved `termios` lives in a `OnceLock`, readable from the handler.
- **Why:** unwinding already runs the guard, so a panic hook would be redundant. A panic on the stdin thread doesn't end the VMM; the terminal is restored when the VM exits.

### D18. Boot parameters
- **Chose:** start `boot_params.hdr` from the kernel's own setup header (as returned by `linux-loader`), then set `boot_flag`, `header`, `type_of_loader = 0xff`, `cmd_line_ptr`, `ramdisk_image/size`, and the three-entry e820 map. Reject kernels whose `xloadflags` lack `XLF_KERNEL_64`.
- **Why:** the boot protocol says the loader should keep the header fields the kernel provides; the 64-bit flag check turns a would-be triple fault into a clear exit-2 error.

### D19. Initrd lower bound
- **Chose:** the initrd must start above `max(kernel_end, load_addr + init_size)`, not just above the loaded bzImage.
- **Why:** the kernel decompresses in place and uses `init_size` bytes; an initrd inside that range would be overwritten during boot.

### D20. `ByteValued` for `boot_params`
- **Chose:** a `#[repr(transparent)]` newtype `BootParams` with a documented `unsafe impl ByteValued`, written with `write_obj`.
- **Why:** `linux-loader 0.14` does not implement `ByteValued` for `boot_params`, and Rust forbids implementing a foreign trait on a foreign type.

### D21. Interrupt-controller details
- **Chose:** `KVM_CREATE_PIT2` with default flags; no LAPIC LINT0/LINT1 programming from the VMM; `KVM_IRQFD` on GSI 4.
- **Why:** Linux programs the LAPIC into virtual-wire mode itself when it finds no MP table (spec pitfall 6); boot and interactive input work without VMM-side LAPIC setup.

### D22. Making AC-3.6 prove something
- **Chose:** the test's wrapper runs `stty echo` before starting the VMM, then checks `icanon` and `echo` in `stty -a` after it exits.
- **Why:** rexpect starts its pty with ECHO off; without `stty echo` a VMM that never restored the terminal would still produce "echo off" and a weaker test.

### D23. M3 tests run one VM at a time
- **Chose:** a `static Mutex<()>` in `tests/m3_linux_boot.rs`; each test holds it for its whole run.
- **Why:** four 512 MiB guests booting in parallel on the nested WSL2 host would make the 30 s / 10 s timing criteria depend on the harness's thread count.
