# Phase 1 spec review — findings, Q1–Q4 answers, proposed spec edits

## Context
You asked for a review of `SPEC.md` and the Phase 1 spec before any planning or code: ambiguities, contradictions, gaps, recommended answers to Q1–Q4, and a list of suggested spec changes. The spec is the source of truth ("Fix the spec, not the code"), so these go into the spec first. Once this is approved, the only step is editing the two SPEC files (plus the repo housekeeping in §5). No Rust code is written in this step.

Host facts I checked (read-only): Intel CPU with `vmx`, `kvm_intel unrestricted_guest=Y`, `pdpe1gb` present, `/dev/kvm` is `root:kvm 0660` and the user is in `kvm`, `nasm` is installed, **`busybox` and `cpio` are not installed**, cargo 1.98.

---

## 1. Contradictions (spec vs. spec, or spec vs. repo)

| # | Issue | Fix |
|---|---|---|
| C1 | **In-kernel irqchip swallows `HLT`.** Once `KVM_CREATE_IRQCHIP` exists, KVM handles HLT in the kernel and never returns `KVM_EXIT_HLT`. A raw guest doing `cli; hlt` then hangs forever. That breaks AC-1.2, AC-2.2 and AC-2.3 ("exits 0 on HLT"). | State that **`raw` mode creates no irqchip and no PIT**. The serial IRQ trigger is a no-op in raw mode. Only `boot` creates the irqchip and PIT. |
| C2 | "Always restore on **Ctrl-C**" contradicts raw mode. Raw mode clears `ISIG`, so Ctrl-C is a byte for the guest and never a SIGINT. | Ctrl-C (0x03) is forwarded to the guest. Rewrite the rule as "restore on any exit path, including SIGINT/SIGTERM/SIGHUP delivered to the VMM process and panics". |
| C3 | Exit-code table says `KVM_EXIT_SHUTDOWN` → 0, but only for `boot`. In `raw` mode a triple fault (bad GDT or page tables) also gives SHUTDOWN, and 0 would hide an M2 bug. | `raw`: SHUTDOWN → exit **1**. `boot`: SHUTDOWN or i8042 reset → 0. |
| C4 | Spec paths say `specs/phase-1/`, but the repo has `specs/phase 1/` (with a space). | Rename the directory to `specs/phase-1/`. |
| C5 | `Cargo.toml` has `edition = "2024"`, but the spec requires 2021. The package is `kvm-hello` with no `[[bin]] vmm`. | Set edition to 2021 and add `[[bin]] name = "vmm"`. This happens at M1 start, not now. |
| C6 | Layout expects `CLAUDE.md` (agent rules), but the repo has an **empty** `Claude.md`. That fails on case-sensitive Linux. | Rename it to `CLAUDE.md`. Its contents are yours to write. |
| C7 | "Output files: exactly these", but Q2 and Q4 need a library target, and none is listed (`src/lib.rs`). `specs/phase-1/PLAN.md` isn't listed either. | Add `src/lib.rs`. Say the list covers deliverables, and workflow docs (PLAN.md) are separate. |
| C8 | AC-0.4 says the message mentions `/dev/kvm`, yet the test swaps in a different path. | "The error message names the device path that failed to open." |

## 2. Ambiguities

- **A1. Raw-mode terminal handling.** Is stdin put into raw mode for `vmm raw`? Raw mode clears `OPOST`, which could change newline output. → Raw terminal mode applies to `boot` only.
- **A2. "Prints exactly … to stdout".** State that the VMM writes nothing to stdout except guest serial bytes. Logs go to stderr (env_logger default), and the default log level is `warn`.
- **A3. UART ports.** The UART covers 8 ports, `0x3f8–0x3ff`, not just `0x3f8`.
- **A4. i8042 read behavior.** Right now reads of `0x60`/`0x64` fall through to `0xFF`. The kernel then sees "output+input buffer full": the i8042 driver reports a stuck controller and `reboot=k` spins in `kb_wait`. → Reads of `0x64` and `0x60` return `0x00`. A write of `0xFE` to `0x64` means reset. Other accesses are ignored.
- **A5. M3 entry protocol.** "Unmodified kernel" doesn't say whether the kernel starts at the 16-bit or the 64-bit entry. → Use the **64-bit boot protocol** (entry = kernel load address + 0x200) and reuse M2's GDT and page tables. That is the whole point of M2, so the spec should say it. The GDT must have `__BOOT_CS=0x10` and `__BOOT_DS=0x18`.
- **A6. `--mem-mib` bounds.** Only `boot < 128` is specified. → `raw`: 1 ≤ N ≤ 3072. `boot`: 128 ≤ N ≤ 3072. The 3 GiB cap keeps RAM below the 32-bit MMIO hole (IOAPIC/LAPIC at `0xFEC00000+`) and below the TSS address, so Phase 1 needs no RAM split. Out of range → exit 2.
- **A7. Raw image size.** Not bounded, so "must not overlap" can't be checked. → Maximum 24 KiB, it must fit in `[0x1000, 0x7000)`, else exit 2. See Q1.
- **A8. `--cmdline` length.** Cap it at 2047 bytes (kernel `COMMAND_LINE_SIZE` 2048, NUL included). Longer → exit 2.
- **A9. Initrd placement and absent initrd.** Say where the initrd goes: top of RAM, page-aligned, at or below `initrd_addr_max`, no overlap with the kernel, else exit 2. Without `--initrd` the kernel panics, `panic=1` reboots, and the VMM exits **0**. Document this as expected: exit 0 means the guest asked to reset, not success.
- **A10. Guest build precondition for `cargo test`.** AC-1.3 and AC-2.4 need `guests/build/*.bin`. Nothing builds them under plain `cargo test`. → The tests fail with a clear "run scripts/build-guests.sh" message. `verify-phase-1.sh` runs `build-guests.sh` first. No `build.rs`.
- **A11. Verify script vs. slow artifacts.** Say whether `verify-phase-1.sh` builds the kernel. → It calls the build scripts only if `artifacts/*` are missing, and both scripts are idempotent.
- **A12. Kernel version pinning.** "6.12 LTS" isn't reproducible. → Pin an exact `6.12.N` tarball from kernel.org and check it against a checked-in SHA-256.
- **A13. AC-3.6 "terminal left in cooked mode".** No check method is given. → The rexpect test spawns `sh -c 'vmm boot …; stty -a'` and asserts `icanon` and `echo` (not `-icanon`/`-echo`) after the VMM exits.
- **A14. AC-3.5 matching.** Match `\n6\.12` on a fresh line so the echoed command text can't match. (AC-3.4's `vmm-$((6*7))` → `vmm-42` is already immune to this. That's a nice design.)
- **A15. `/init` shell.** With a plain `exec /bin/sh`, BusyBox has no controlling tty ("job control turned off") and Ctrl-C won't reach commands. → Use `exec setsid cttyhack /bin/sh` with `PS1` exported, and have the build script confirm those applets exist in Ubuntu's `busybox-static`.
- **A16. `unwrap` on `Mutex::lock`.** Say whether it's allowed. → A poisoned lock is an internal error (exit 1). No `unwrap`.

## 3. Missing items

- **M1. Host prerequisites.** List them: `nasm`, `busybox-static`, `cpio`, and the kernel build deps (`build-essential flex bison bc libelf-dev libssl-dev`). **busybox-static and cpio are not installed on this host.**
- **M2. KVM capability checks.** Check API version 12, `KVM_CAP_IRQCHIP`, `KVM_CAP_IRQFD` and `KVM_CAP_USER_MEMORY`. Missing → exit 2.
- **M3. `KVM_RUN` returning `EINTR`/`EAGAIN`.** Retry it. It is not an unexpected exit.
- **M4. Test timeouts.** A raw guest that never halts hangs `cargo test`. → Each spawned VMM gets a timeout: M1/M2 10 s, M3 per the AC timings. Kill it on expiry and fail.
- **M5. Exit on signal.** SIGTERM/SIGHUP/SIGINT (when not raw) → restore the terminal and exit **1** with "terminated by signal". Everything stays inside the 0/1/2 table.
- **M6. Guest-controlled arithmetic.** "Nothing may panic" also covers debug-build overflow and slice indexing. Require checked or wrapping arithmetic and `.get()` on anything derived from exit data.
- **M7. Enforce via lints.** Add `#![deny(clippy::undocumented_unsafe_blocks, clippy::unwrap_used, clippy::expect_used)]`. The first lint subsumes AC-0.3's grep, which misses `unsafe impl`, e.g. `ByteValued` for `boot_params`. Tests may `#![allow]` unwrap.
- **M8. Serial input backpressure.** vm-superio's RX FIFO is 64 bytes and `enqueue_raw_bytes` errors when full. The stdin thread must wait and retry, never drop or panic. On stdin EOF, stop reading and keep the guest running.
- **M9. Known pitfalls to add.** (6) With no MP table and no ACPI, Linux runs the LAPIC in virtual-wire mode (LINT0 = ExtINT) and interrupts reach it through the PIC. IRQ 4 goes through `KVM_IRQFD` on GSI 4, and default routing sends it to both PIC and IOAPIC. The spec should say the VMM writes no MP table in Phase 1. If interrupts don't arrive, adding one requires a spec change first. (7) The TSS address (`0xfffbd000`) must be outside guest RAM, which the 3 GiB cap guarantees. (8) Intel real mode relies on unrestricted guest. It's `Y` on this host; the plan should note it.
- **M10. Crate versions.** `linux-loader`, `kvm-ioctls` and `vm-superio` pin specific `vm-memory` and `kvm-bindings` versions. The plan picks one compatible set, and `Cargo.lock` records it.

---

## 4. Answers to Q1–Q4

### Q1 — Guest memory layout (the same map serves M2 and M3)
| GPA | Contents |
|---|---|
| `0x0500` | GDT: null, null, code64 `0x10` (L=1), data `0x18`, TSS `0x20` (Linux boot protocol selectors) |
| `0x0520` | IDT, limit 0. Interrupts stay off until the guest installs its own. |
| `0x1000–0x6FFF` | Raw image (M1/M2), max 24 KiB |
| `0x7000` | `boot_params` zero page (M3) |
| `0x8FF0` | Initial RSP (stack grows down, below the page tables) |
| `0x9000` / `0xA000` / `0xB000` | PML4 / PDPT / PD: 512 × 2 MiB pages = 1 GiB identity map |
| `0x20000` | Kernel command line (M3) |
| `0x100000` | bzImage protected-mode kernel (M3, via `linux-loader`) |
| top of RAM | initrd (A9) |

e820 (M3): `[0, 0x9FC00)` RAM, `[0x9FC00, 0x100000)` reserved, `[0x100000, mem_end)` RAM.

I use 2 MiB pages, not 1 GiB pages. `pdpe1gb` exists here, but it isn't guaranteed under nested virtualization, and 2 MiB pages cost only one extra page. The layout follows the widely documented Linux/Firecracker conventions, but the values are chosen and written from scratch.

### Q2 — Simulating a missing `/dev/kvm` (AC-0.4)
Add a hidden global CLI option, `--kvm-device <PATH>` (`#[arg(hide = true)]`, default `/dev/kvm`), passed to `Kvm::new_with_path`. The test spawns the real binary with `--kvm-device /nonexistent/kvm` and checks exit code 2 plus the path in stderr, which is end to end with no mocking. The option is harmless: the VMM runs as the user, so pointing it elsewhere grants nothing.
Rejected: user-namespace tricks (Ubuntu 24.04's AppArmor restricts unprivileged userns) and an env-var override (invisible and easy to leave set by accident).

### Q3 — Reading stdin without blocking the vCPU
Use a **dedicated stdin thread**. `epoll` alone can't do it, because `KVM_RUN` blocks the calling thread and the loop would need signal kicks (`immediate_exit`) to wake it.
- The main thread sets up everything, then runs the vCPU loop.
- The stdin thread does a blocking `read`, then locks `Arc<Mutex<Serial>>` and `enqueue_raw_bytes`, which fires the IRQ-4 `EventFd` → irqfd → guest (pitfall 5). If the FIFO is full, it retries per M8.
- At VM exit the main thread restores the terminal and exits the process without joining the stdin thread, which is stuck in `read`. A Drop guard, a panic hook and a signal handler all restore termios using `vmm_sys_util::terminal` (already on the allowed-crates list).

### Q4 — Exposing sregs for AC-2.3 without test-only code
Split into a **library plus a thin binary**. `src/lib.rs` exposes something like `run_raw(cfg, out: impl Write) -> Result<RawOutcome>`, where `RawOutcome` includes the final `kvm_sregs` after HLT. `main.rs` only parses the CLI, calls the library, writes output and maps errors to exit codes.
- `tests/m2_long_mode.rs` calls `run_raw` in process with a `Vec<u8>` sink and asserts `efer & LMA` and `cs.l == 1`, plus the output bytes.
- Returning final vCPU state is an ordinary API, so no `cfg(test)` or feature flags are needed and nothing test-only ships.
- AC-1.2 and AC-2.2 still spawn the real binary (`env!("CARGO_BIN_EXE_vmm")`) to check real stdout and exit codes.

---

## 5. Proposed spec edits (to apply once approved)

**`SPEC.md` (project):**
1. Fix the layout wording to `specs/phase-1/` and `CLAUDE.md`. No other changes are needed there.

**`specs/phase-1/SPEC.md`:**
1. *Architecture limits:* irqchip and PIT only in `boot` (C1). UART range `0x3f8–0x3ff` (A3). i8042 read/write behavior (A4). No MP table (M9).
2. *Behavioral rules:* rewrite the Ctrl-C clause (C2). Stdout carries only guest bytes and logs go to stderr at `warn` (A2). Raw terminal mode only for `boot` (A1). Retry `EINTR` (M3). Checked arithmetic (M6). No `unwrap` on locks (A16). Serial backpressure and EOF (M8). Signal exit (M5).
3. *Stack:* add the lint `deny` list (M7) and a host prerequisites list (M1). Pin the kernel to an exact `6.12.N` plus SHA-256 (A12).
4. *CLI table:* `--mem-mib` ranges (A6). `--cmdline` ≤ 2047 bytes (A8). Hidden `--kvm-device` (Q2). Raw image ≤ 24 KiB (A7).
5. *Raw-guest contract:* the layout table from Q1. SHUTDOWN → exit 1 in raw mode (C3). Initial RSP.
6. *Linux boot contract:* 64-bit entry reusing the M2 setup (A5). Initrd placement, and document no-initrd → exit 0 (A9). e820 table. The `/init` line uses `setsid cttyhack` (A15). Capability checks → exit 2 (M2).
7. *Exit codes:* clarify the SHUTDOWN split and signal → 1.
8. *ACs:* AC-0.3 enforced by clippy (grep kept as a backup). AC-0.4 wording (C8). Build precondition for AC-1.3 and AC-2.4 (A10). Timeouts (M4). AC-3.5 regex (A14). AC-3.6 check method (A13). The verify script builds missing artifacts (A11).
9. *Output files:* add `src/lib.rs` and clarify that PLAN.md is excluded (C7).
10. *Known pitfalls:* add items 6–8 (M9) and the crate-version note (M10).
11. *Open questions:* replace Q1–Q4 with their resolutions from §4.

**Repo housekeeping (with the spec edit):** `git mv "specs/phase 1" specs/phase-1` and `git mv Claude.md CLAUDE.md`. The Cargo.toml fixes (C5) and `.gitignore` entries wait for M1, per the no-scope-creep rule.

## Verification
- Reread both spec files after editing. Every item in §5 should appear, and no remaining text should contradict it, e.g. grep for "Ctrl-C", "SHUTDOWN", "phase 1", "/dev/kvm".
- Every AC is still pass/fail and machine-checkable.
- `git status` shows only the two spec files and the two renames changed.
