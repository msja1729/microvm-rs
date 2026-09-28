# Phase 1 SPEC — Boot a guest

**Status:** ACTIVE
**Depends on:** nothing (first phase)
**Verified by:** `scripts/verify-phase-1.sh` (all milestones green)

---

## Goal
A single Rust binary, `vmm`, that uses KVM to boot an unmodified x86_64 Linux kernel to an interactive BusyBox shell over an emulated serial console, with one vCPU and configurable memory.

Phase 1 is delivered in three milestones, implemented and verified **in order**:

| Milestone | Outcome |
|---|---|
| M1 — Real-mode hello | A 16-bit raw guest prints a string through the serial port and halts |
| M2 — Long-mode hello | The VMM puts the vCPU into 64-bit long mode; a 64-bit raw guest prints a string and halts |
| M3 — Linux boot | An unmodified Linux kernel + initramfs boots to a BusyBox shell; the user can type commands; `reboot -f` exits the VMM cleanly |

---

## Constraints

### Stack
- Rust stable, edition 2021. Binary name `vmm` (set via `[[bin]]` in `Cargo.toml`).
- Allowed runtime crates: `kvm-ioctls`, `kvm-bindings`, `vm-memory` (feature `backend-mmap`), `linux-loader`, `vm-superio`, `vmm-sys-util`, `libc`, `clap` (derive), `anyhow`, `thiserror`, `log`, `env_logger`.
- Allowed dev-only crates: `rexpect` (driving the interactive shell in tests), `tempfile`.
- Any other crate requires updating this spec first.
- Guest assembly: `nasm`, flat binary output (`-f bin`).
- Guest kernel: Linux **6.12 LTS**, built from `make x86_64_defconfig` + `make kvm_guest.config`.
- Guest userspace: static BusyBox from the Ubuntu `busybox-static` package, packed into a cpio initramfs.
- Kernel version is pinned to one exact `6.12.N` release from kernel.org; `scripts/build-kernel.sh` verifies the tarball against a SHA-256 stored in the script.
- Crate-level lints in `src/lib.rs` and `src/main.rs`: `#![deny(clippy::undocumented_unsafe_blocks, clippy::unwrap_used, clippy::expect_used)]` (tests may `allow` unwrap/expect).
- `linux-loader`, `kvm-ioctls` and `vm-superio` depend on specific `vm-memory` / `kvm-bindings` versions; the plan picks one compatible set and `Cargo.lock` records it.

### Host prerequisites
`nasm`, `busybox-static`, `cpio`, and the kernel build dependencies `build-essential flex bison bc libelf-dev libssl-dev`. Scripts check for them and fail with the exact `apt install` command if missing.

### Architecture limits (Phase 1 only)
- Exactly one vCPU.
- No PCI, no ACPI, no virtio, no disk, no network.
- In-kernel interrupt controller and timer: KVM irqchip (PIC/IOAPIC/LAPIC) and KVM PIT — **`boot` mode only**. `raw` mode creates neither, because with an in-kernel irqchip KVM handles `HLT` itself and never returns `KVM_EXIT_HLT` to the VMM. In `raw` mode the serial IRQ trigger is a no-op.
- Devices emulated by the VMM: a 16550 UART at I/O ports `0x3f8`–`0x3ff`, IRQ 4 (via `vm-superio`); and the minimum of an i8042 controller needed to detect a guest reset request.
- i8042 behavior: reads of port `0x60` and `0x64` return `0x00` (no data pending, input buffer empty); a write of `0xFE` to `0x64` is a reset request; every other i8042 access is ignored.
- No MP table and no ACPI tables. Linux runs the LAPIC in virtual-wire mode and receives legacy IRQs through the PIC. If interrupts cannot be delivered this way, adding an MP table requires a spec change first.

### Behavioral rules
- Unhandled port-I/O reads return all-ones (`0xFF` per byte); unhandled writes are ignored. Both are logged at `debug` level only. Linux probes many ports at boot, so this must never be an error.
- Unhandled MMIO accesses follow the same rule.
- Any other unexpected VM exit: print the exit reason to stderr and exit with code 1.
- `KVM_RUN` failing with `EINTR` or `EAGAIN` is retried; it is not an unexpected exit.
- stdout carries **only** bytes the guest wrote to the serial port. All VMM diagnostics and logs go to stderr; the default log level is `warn` (override with `RUST_LOG`).
- `boot` only: when stdin is a terminal, put it in raw mode while the guest runs and **always restore it** on every exit path — normal exit, error exit, panic, and SIGINT/SIGTERM/SIGHUP delivered to the VMM process. In raw mode Ctrl-C is an ordinary byte (0x03) forwarded to the guest. `raw` mode never changes terminal settings and does not read stdin.
- Receiving SIGINT/SIGTERM/SIGHUP: restore the terminal, print `terminated by signal` to stderr, exit with code 1.
- Serial input: the guest RX FIFO is 64 bytes. When it is full, host input waits and retries — bytes are never dropped and a full FIFO never panics. On stdin EOF the VMM stops reading input and keeps the guest running.
- No `unwrap()` / `expect()` on paths reachable from guest behavior or user input. A poisoned `Mutex` is an internal error (exit 1), not an `unwrap`.
- Values derived from guest exits (port numbers, offsets, sizes, data) use checked or wrapping arithmetic and non-panicking indexing (`.get()`), so debug builds cannot overflow-panic either.

### Performance bound
- M3: from process start to the init-ready marker (see Inputs and outputs) in **≤ 30 s** on the WSL2 development host. This is generous on purpose; nested virtualization is slow.

---

## Inputs and outputs

### CLI
```
vmm raw  --image <path.bin> --mode <real|long> [--mem-mib <N>]      # M1, M2
vmm boot --kernel <bzImage> [--initrd <path.cpio>] [--mem-mib <N>] [--cmdline "<str>"]   # M3
```
| Flag | Default | Notes |
|---|---|---|
| `--mem-mib` | `16` for `raw`, `512` for `boot` | `raw`: 1–3072. `boot`: 128–3072. Out of range → exit 2. The 3 GiB cap keeps RAM below the 32-bit MMIO hole and the TSS address. |
| `--cmdline` | `console=ttyS0 reboot=k panic=1 pci=off acpi=off` | At most 2047 bytes, else exit 2 |
| `--kvm-device` | `/dev/kvm` | Hidden global option (not shown in `--help`); path of the KVM device to open. Exists so AC-0.4 can be tested end to end. |

### Guest memory layout (M1–M3)
| Guest-physical address | Contents |
|---|---|
| `0x0500` | GDT: null, null, code64 `0x10`, data `0x18`, TSS `0x20` (Linux boot protocol selectors) |
| `0x0520` | IDT, limit 0 |
| `0x1000`–`0x6FFF` | Raw image (M1, M2), at most 24 KiB |
| `0x7000` | `boot_params` zero page (M3) |
| `0x8FF0` | Initial stack pointer (stack grows down) |
| `0x9000` / `0xA000` / `0xB000` | PML4 / PDPT / PD: 512 × 2 MiB pages = identity map of the first 1 GiB |
| `0x20000` | Kernel command line (M3) |
| `0x100000` | bzImage protected-mode kernel (M3) |
| top of RAM | initrd (M3) |

The KVM TSS region (`KVM_SET_TSS_ADDR`) is at `0xfffbd000`, always outside guest RAM because of the 3 GiB cap.

### Raw-guest contract (M1, M2)
- The image is loaded at guest-physical address **`0x1000`**; execution starts at its first byte. An empty image or one larger than 24 KiB is rejected with exit 2.
- `--mode real`: vCPU starts in 16-bit real mode with all segment selectors and bases 0, IP = `0x1000`, SP = `0x8FF0`, interrupts disabled.
- `--mode long`: vCPU starts in 64-bit long mode using the GDT and page tables from the memory layout above, RIP = `0x1000`, RSP = `0x8FF0`, interrupts disabled.
- Guest writes to the UART transmit register (port `0x3f8`) → the byte goes to host stdout.
- Guest `HLT` → VMM exits with code **0**.
- `KVM_EXIT_SHUTDOWN` (e.g. a triple fault) → exit code **1**; in `raw` mode it always indicates a bug.

### Linux boot contract (M3)
- The VMM checks KVM API version 12 and `KVM_CAP_USER_MEMORY` (all modes), plus `KVM_CAP_IRQCHIP` and `KVM_CAP_IRQFD` (`boot`). A missing capability is a host setup error (exit 2).
- Kernel format: bzImage, loaded using the Linux x86 boot protocol (via `linux-loader`), with an e820 memory map, the command line, and the initrd location in the boot parameters.
- Entry uses the **64-bit boot protocol**: the vCPU starts in long mode with the M2 GDT and page tables, RIP = kernel load address + `0x200`, RSI = address of `boot_params`.
- e820 map: `[0, 0x9FC00)` RAM, `[0x9FC00, 0x100000)` reserved, `[0x100000, mem_end)` RAM.
- The initrd is placed at the top of RAM, page-aligned, at or below the kernel's `initrd_addr_max`, not overlapping the kernel; if it does not fit → exit 2.
- Without `--initrd` the kernel panics, `panic=1` reboots, and the VMM exits 0. This is expected: exit 0 means the guest requested a reset, not that it succeeded.
- The initramfs `/init` script: mounts `proc`, `sysfs`, `devtmpfs`; prints the marker line **`PHASE1-INIT-OK`**; exports `PS1='phase1# '`; then runs `exec setsid cttyhack /bin/sh` so the shell has a controlling terminal.
- Serial output → host stdout. Host stdin → serial input (the guest receives keystrokes).
- Guest reset request (write `0xFE` to i8042 port `0x64`) **or** `KVM_EXIT_SHUTDOWN` → VMM exits with code **0**.

### Exit codes
| Code | Meaning |
|---|---|
| 0 | Guest halted (raw) or reset/shut down (boot) |
| 1 | Unexpected VM exit (including `KVM_EXIT_SHUTDOWN` in raw mode), internal error, or terminated by signal (message on stderr) |
| 2 | Host setup error: KVM device missing or not accessible, missing KVM capability, file not found, image/initrd too large, invalid CLI arguments |

---

## Acceptance criteria
Every criterion is pass/fail and is checked by `scripts/verify-phase-1.sh`.
Every test that spawns the VMM enforces a timeout (M1/M2: 10 s; M3: the times stated below), kills the VMM on expiry and fails.

### Always (every milestone)
- **AC-0.1** `cargo fmt --check` passes.
- **AC-0.2** `cargo clippy --all-targets -- -D warnings` passes.
- **AC-0.3** Every `unsafe` block and `unsafe impl` in `src/` has a `// SAFETY:` comment directly above it. Enforced by `clippy::undocumented_unsafe_blocks` (so AC-0.2 covers it), with a grep in the verify script as a backup.
- **AC-0.4** Running `vmm --kvm-device /nonexistent/kvm raw …` prints an error naming the device path that failed to open and exits with code 2.

### M1 — Real-mode hello
- **AC-1.1** `guests/hello16.asm` assembles with `nasm -f bin`.
- **AC-1.2** `vmm raw --image guests/build/hello16.bin --mode real` prints exactly `Hello from real mode!\n` to stdout and exits with code 0.
- **AC-1.3** Integration test `tests/m1_real_mode.rs` asserts AC-1.2 and passes under `cargo test`.
- Precondition for AC-1.3 and AC-2.4: `scripts/build-guests.sh` has been run. If `guests/build/*.bin` is missing, the tests fail with a message saying to run it. `verify-phase-1.sh` runs it first.

### M2 — Long-mode hello
- **AC-2.1** `guests/hello64.asm` is written for `bits 64` and uses at least one 64-bit-only register (e.g. `r8`) in producing its output, so it cannot work if the vCPU is not in long mode.
- **AC-2.2** `vmm raw --image guests/build/hello64.bin --mode long` prints exactly `Hello from long mode!\n` and exits with code 0.
- **AC-2.3** After the guest halts, an integration test reads the vCPU's special registers and asserts `EFER.LMA = 1` and `CS.L = 1`. The library's raw-run API returns the final special registers as part of its normal result; the test calls it in process.
- **AC-2.4** `tests/m2_long_mode.rs` covers AC-2.2 and AC-2.3 and passes under `cargo test`.
- **AC-2.5** M1 tests still pass (no regression).

### M3 — Linux boot
Kernel and initramfs are built by scripts (below). M3 tests are `#[ignore]`d by default because they need those artifacts and run with `cargo test -- --ignored`. `verify-phase-1.sh` runs the build scripts only if `artifacts/*` are missing; both scripts are idempotent.
- **AC-3.1** `scripts/build-kernel.sh` produces `artifacts/bzImage` from Linux 6.12 LTS.
- **AC-3.2** `scripts/build-initramfs.sh` produces `artifacts/initramfs.cpio` meeting the `/init` contract above.
- **AC-3.3** `vmm boot --kernel artifacts/bzImage --initrd artifacts/initramfs.cpio` prints `PHASE1-INIT-OK` within 30 s.
- **AC-3.4** The shell prompt `phase1# ` appears; sending `echo vmm-$((6*7))` prints `vmm-42`. (This proves keyboard input reaches the guest, not just output.)
- **AC-3.5** Sending `uname -r` prints a version string starting with `6.12`, matched at the start of a line (`\n6\.12`) so the echoed command cannot match.
- **AC-3.6** Sending `reboot -f` makes the VMM exit with code 0 within 10 s, and the host terminal is left in its original (cooked) mode. Checked by spawning `sh -c 'vmm boot …; stty -a'` under rexpect and asserting `icanon` and `echo` (not `-icanon` / `-echo`) after the VMM exits.
- **AC-3.7** `tests/m3_linux_boot.rs` automates AC-3.3 – AC-3.6 using `rexpect`.
- **AC-3.8** M1 and M2 tests still pass.

### Phase 1 done
- **AC-P1** `scripts/verify-phase-1.sh` runs AC-0.x, M1, M2, and M3 checks, prints a PASS/FAIL line per criterion, and exits 0 only if every criterion passes.
- **AC-P2** `notes/phase-1.md` exists and contains: an explanation of each milestone in plain language, the VM-exit handling flow, and at least 10 interview-style questions with answers.

---

## Output files
The agent produces exactly these deliverable files (plus `Cargo.lock`). Adding or removing a file requires updating this list first. Workflow documents (`specs/phase-1/PLAN.md`, `specs/phase-1/DECISIONS.md`) are not part of this list.

```
Cargo.toml
src/main.rs                    # CLI (clap), exit-code mapping — thin wrapper over the library
src/lib.rs                     # library entry points (run raw guest, boot kernel)
src/error.rs                   # error types
src/vm.rs                      # VM creation, guest memory, irqchip, PIT
src/vcpu.rs                    # vCPU creation and the run loop / exit dispatch
src/loader.rs                  # raw image loading; bzImage + initrd loading
src/arch/mod.rs
src/arch/x86_64/mod.rs
src/arch/x86_64/regs.rs        # real-mode and long-mode register setup, CPUID
src/arch/x86_64/gdt.rs         # GDT entries for long mode
src/arch/x86_64/paging.rs      # identity-mapped page tables
src/arch/x86_64/boot_params.rs # zero page, e820 map, cmdline placement
src/devices/mod.rs
src/devices/pio_bus.rs         # port-I/O dispatch, default all-ones/ignore behavior
src/devices/serial.rs          # 16550 via vm-superio, IRQ 4, stdin input
src/devices/i8042.rs           # reset detection only
src/terminal.rs                # raw-mode enable + guaranteed restore
guests/hello16.asm
guests/hello64.asm
scripts/build-guests.sh        # nasm → guests/build/*.bin
scripts/build-kernel.sh        # fetch 6.12 LTS, configure, build → artifacts/bzImage
scripts/build-initramfs.sh     # busybox-static + /init → artifacts/initramfs.cpio
scripts/verify-phase-1.sh      # runs every acceptance criterion
tests/m1_real_mode.rs
tests/m2_long_mode.rs
tests/m3_linux_boot.rs
notes/phase-1.md
.gitignore                     # ignores target/, guests/build/, artifacts/
```

---

## Out of scope for Phase 1
Multiple vCPUs, virtio devices, disks, networking, snapshots, seccomp/jailer, SEV/TDX, ACPI, PCI, performance tuning, a config file format. Do not add them, even partially.

---

## Known pitfalls — PLAN.md must address each one
These are facts about KVM and x86, not implementation choices:
1. On Intel hosts, `KVM_SET_TSS_ADDR` must be called before the first `KVM_RUN`.
2. The in-kernel irqchip must be created **before** any vCPU is created.
3. The vCPU needs a CPUID table (from `KVM_GET_SUPPORTED_CPUID`) set before Linux will boot.
4. Guest-visible memory layout: the kernel, initrd, boot params, command line, page tables, and GDT must not overlap, and the e820 map must describe usable RAM correctly.
5. Serial interrupts must actually be delivered to the guest (irqfd/eventfd), or the interactive shell will hang even though boot output appears.
6. With no MP table and no ACPI, Linux uses the LAPIC in virtual-wire mode (LINT0 = ExtINT) and gets legacy IRQs from the PIC. IRQ 4 is injected with `KVM_IRQFD` on GSI 4; KVM's default routing sends GSIs 0–15 to both PIC and IOAPIC.
7. The `KVM_SET_TSS_ADDR` region (`0xfffbd000`, 3 pages) must not overlap guest RAM.
8. On Intel, real mode relies on the `unrestricted_guest` feature of `kvm_intel` (it is `Y` on the development host).

## Resolved questions
- Q1. Where exactly do the long-mode page tables and GDT live in guest memory? → See *Guest memory layout*: GDT `0x500`, IDT `0x520`, PML4/PDPT/PD at `0x9000`/`0xA000`/`0xB000` with 2 MiB pages (1 GiB pages are not guaranteed under nested virtualization).
- Q2. How does the test for AC-0.4 simulate a missing `/dev/kvm` without breaking the real path? → Hidden `--kvm-device <PATH>` option (default `/dev/kvm`); the test passes a non-existent path to the real binary.
- Q3. How does the run loop read host stdin without blocking vCPU execution? → A dedicated stdin thread feeds the shared serial device (behind a `Mutex`) and fires the IRQ-4 eventfd; the vCPU loop runs on the main thread. `epoll` alone cannot work because `KVM_RUN` blocks its thread.
- Q4. How are AC-2.3's register values exposed to the test without leaking test-only code into normal runs? → Library + thin binary: the library's raw-run function returns the final special registers as part of its normal result, and the test calls it in process.
