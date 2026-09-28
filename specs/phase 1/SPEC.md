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

### Architecture limits (Phase 1 only)
- Exactly one vCPU.
- No PCI, no ACPI, no virtio, no disk, no network.
- In-kernel interrupt controller and timer: KVM irqchip (PIC/IOAPIC/LAPIC) and KVM PIT.
- Devices emulated by the VMM: a 16550 UART at I/O port `0x3f8`, IRQ 4 (via `vm-superio`); and the minimum of an i8042 controller needed to detect a guest reset request.

### Behavioral rules
- Unhandled port-I/O reads return all-ones (`0xFF` per byte); unhandled writes are ignored. Both are logged at `debug` level only. Linux probes many ports at boot, so this must never be an error.
- Unhandled MMIO accesses follow the same rule.
- Any other unexpected VM exit: print the exit reason to stderr and exit with code 1.
- When stdin is a terminal, put it in raw mode while the guest runs and **always restore it** on exit, including error exits and Ctrl-C.
- No `unwrap()` / `expect()` on paths reachable from guest behavior or user input.

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
| `--mem-mib` | `16` for `raw`, `512` for `boot` | `boot` rejects values `< 128` with an error |
| `--cmdline` | `console=ttyS0 reboot=k panic=1 pci=off acpi=off` | |

### Raw-guest contract (M1, M2)
- The image is loaded at guest-physical address **`0x1000`**; execution starts at its first byte.
- `--mode real`: vCPU starts in 16-bit real mode with segment bases 0 and IP = `0x1000`.
- `--mode long`: vCPU starts in 64-bit long mode with an identity mapping covering at least the first 1 GiB, RIP = `0x1000`. The page tables and GDT must not overlap the loaded image.
- Guest writes to port `0x3f8` → the byte goes to host stdout.
- Guest `HLT` → VMM exits with code **0**.

### Linux boot contract (M3)
- Kernel format: bzImage, loaded using the Linux x86 boot protocol (via `linux-loader`), with an e820 memory map, the command line, and the initrd location in the boot parameters.
- The initramfs `/init` script: mounts `proc`, `sysfs`, `devtmpfs`; prints the marker line **`PHASE1-INIT-OK`**; sets `PS1='phase1# '`; then starts `/bin/sh` on the console.
- Serial output → host stdout. Host stdin → serial input (the guest receives keystrokes).
- Guest reset request (write `0xFE` to i8042 port `0x64`) **or** `KVM_EXIT_SHUTDOWN` → VMM exits with code **0**.

### Exit codes
| Code | Meaning |
|---|---|
| 0 | Guest halted (raw) or reset/shut down (boot) |
| 1 | Unexpected VM exit or internal error (message on stderr) |
| 2 | Host setup error: `/dev/kvm` missing or not accessible, file not found, invalid CLI arguments |

---

## Acceptance criteria
Every criterion is pass/fail and is checked by `scripts/verify-phase-1.sh`.

### Always (every milestone)
- **AC-0.1** `cargo fmt --check` passes.
- **AC-0.2** `cargo clippy --all-targets -- -D warnings` passes.
- **AC-0.3** Every `unsafe` block in `src/` has a `// SAFETY:` comment on the line(s) directly above it (checked by a grep in the verify script).
- **AC-0.4** Running `vmm` when `/dev/kvm` cannot be opened prints an error mentioning `/dev/kvm` and exits with code 2 (tested by pointing the VMM at a non-existent device path via a test-only override, or equivalent — the plan decides the mechanism).

### M1 — Real-mode hello
- **AC-1.1** `guests/hello16.asm` assembles with `nasm -f bin`.
- **AC-1.2** `vmm raw --image guests/build/hello16.bin --mode real` prints exactly `Hello from real mode!\n` to stdout and exits with code 0.
- **AC-1.3** Integration test `tests/m1_real_mode.rs` asserts AC-1.2 and passes under `cargo test`.

### M2 — Long-mode hello
- **AC-2.1** `guests/hello64.asm` is written for `bits 64` and uses at least one 64-bit-only register (e.g. `r8`) in producing its output, so it cannot work if the vCPU is not in long mode.
- **AC-2.2** `vmm raw --image guests/build/hello64.bin --mode long` prints exactly `Hello from long mode!\n` and exits with code 0.
- **AC-2.3** After the guest halts, an integration test reads the vCPU's special registers and asserts `EFER.LMA = 1` and `CS.L = 1`. (The VMM exposes whatever test hook is needed; the plan decides how.)
- **AC-2.4** `tests/m2_long_mode.rs` covers AC-2.2 and AC-2.3 and passes under `cargo test`.
- **AC-2.5** M1 tests still pass (no regression).

### M3 — Linux boot
Kernel and initramfs are built by scripts (below). M3 tests are `#[ignore]`d by default because they need those artifacts and run with `cargo test -- --ignored`.
- **AC-3.1** `scripts/build-kernel.sh` produces `artifacts/bzImage` from Linux 6.12 LTS.
- **AC-3.2** `scripts/build-initramfs.sh` produces `artifacts/initramfs.cpio` meeting the `/init` contract above.
- **AC-3.3** `vmm boot --kernel artifacts/bzImage --initrd artifacts/initramfs.cpio` prints `PHASE1-INIT-OK` within 30 s.
- **AC-3.4** The shell prompt `phase1# ` appears; sending `echo vmm-$((6*7))` prints `vmm-42`. (This proves keyboard input reaches the guest, not just output.)
- **AC-3.5** Sending `uname -r` prints a version string starting with `6.12`.
- **AC-3.6** Sending `reboot -f` makes the VMM exit with code 0 within 10 s, and the host terminal is left in its original (cooked) mode.
- **AC-3.7** `tests/m3_linux_boot.rs` automates AC-3.3 – AC-3.6 using `rexpect`.
- **AC-3.8** M1 and M2 tests still pass.

### Phase 1 done
- **AC-P1** `scripts/verify-phase-1.sh` runs AC-0.x, M1, M2, and M3 checks, prints a PASS/FAIL line per criterion, and exits 0 only if every criterion passes.
- **AC-P2** `notes/phase-1.md` exists and contains: an explanation of each milestone in plain language, the VM-exit handling flow, and at least 10 interview-style questions with answers.

---

## Output files
The agent produces exactly these files (plus `Cargo.lock`). Adding or removing a file requires updating this list first.

```
Cargo.toml
src/main.rs                    # CLI (clap), exit-code mapping
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

## Open questions — resolve during planning, then update this spec
- Q1. Where exactly do the long-mode page tables and GDT live in guest memory?
- Q2. How does the test for AC-0.4 simulate a missing `/dev/kvm` without breaking the real path?
- Q3. How does the run loop read host stdin without blocking vCPU execution (separate thread vs. `epoll`)?
- Q4. How are AC-2.3's register values exposed to the test without leaking test-only code into normal runs?