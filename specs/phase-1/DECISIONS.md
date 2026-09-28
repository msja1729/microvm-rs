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
