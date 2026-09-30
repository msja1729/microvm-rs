# Phase 2 SPEC — A real VMM

**Status:** ACTIVE
**Depends on:** Phase 1 (tag `phase-1-complete`)
**Verified by:** `scripts/verify-phase-2.sh` (all milestones green, and `scripts/verify-phase-1.sh` still green)

---

## Goal
Extend `vmm` so a Linux guest gets a persistent disk, a working network, and multiple vCPUs, with the VMM hardened against a malicious guest by seccomp filters and with its guest-facing parsing code checked by Kani proofs and fuzzing.

Phase 2 is delivered in five milestones, implemented and verified **in order**. Milestone numbers continue from Phase 1.

| Milestone | Outcome |
|---|---|
| M4 — virtio-blk | virtio-mmio transport + a block device backed by a host file; the guest mounts an ext4 disk and its writes persist across reboots |
| M5 — virtio-net | A network device backed by a host TAP interface; guest and host can ping each other and transfer files |
| M6 — Multiple vCPUs | `--vcpus 1..4`; the guest sees and uses every vCPU |
| M7 — Hardening | Every VMM thread runs under a seccomp allowlist; a forbidden syscall kills the VMM |
| M8 — Verification | Kani proofs and a fuzz target show the guest-facing queue and request parsers cannot panic, overflow, loop forever, or access memory out of bounds |

---

## Constraints

### Stack
- Everything in the Phase 1 spec's Stack section still applies.
- Newly allowed runtime crates: `virtio-bindings`, `seccompiler`, `event-manager`. The split virtqueue is implemented in this crate, not taken from `virtio-queue` (see Resolved questions, Q2).
- `seccompiler`, `event-manager` and `virtio-bindings` must work with the Phase 1 `vm-memory` / `kvm-bindings` / `vmm-sys-util` versions; the plan picks one compatible set, `Cargo.lock` records it, and `cargo tree -d` shows no duplicated rust-vmm crates.
- Newly allowed tools (installed without sudo): a nightly Rust toolchain (only for fuzzing), `cargo-fuzz`, and Kani (`cargo install --locked kani-verifier && cargo kani setup`). The fuzz target lives in a separate `fuzz/` crate that may depend on `libfuzzer-sys`. `fuzz/` is its own Cargo workspace, not a member of the root package, so root `cargo build/test/clippy` never builds it.
- Kani harnesses are gated by `#[cfg(kani)]`. `Cargo.toml` declares `[lints.rust] unexpected_cfgs = { level = "warn", check-cfg = ['cfg(kani)'] }` so `cargo clippy --all-targets -- -D warnings` accepts the gate. The `kani` crate is provided by `cargo kani`, not listed as a dependency.
- Any other crate requires updating this spec first.
- Guest kernel: still Linux 6.12 LTS, built from `x86_64_defconfig` + `kvm_guest.config` + a committed fragment `configs/phase2.config` that enables at least `VIRTIO_MMIO`, `VIRTIO_MMIO_CMDLINE_DEVICES`, `VIRTIO_BLK`, `VIRTIO_NET`, `EXT4_FS`, and `X86_MPPARSE` as built-in (`=y`).
- `scripts/build-kernel.sh` rebuilds whenever the kernel version **or** the SHA-256 of `configs/phase2.config` differs from the stamp in `artifacts/bzImage.version`; the stamp records both.

### Host prerequisites
Everything from Phase 1, plus: `e2fsprogs` (`mkfs.ext4` with `-d`, and `debugfs`), `python3`, `iproute2` (`ip`), `iputils-ping`, `strace` (for deriving seccomp allowlists), and `/dev/net/tun` (present in the WSL2 kernel). Scripts check for them and fail with the exact `apt install` command if missing.

### Architecture limits (Phase 2 only)
- Device transport: virtio-mmio only. Still no PCI and no ACPI.
- Up to 4 vCPUs. SMP is described to the guest with an MP table (not ACPI).
- In `boot` mode the VMM **always** writes an MP table, even with `--vcpus 1`, so the guest always routes legacy and virtio IRQs through the IOAPIC. This replaces the Phase 1 "no MP table; IRQs through the PIC" rule; AC-0.5 checks that Phase 1 still passes. The MP table is placed in the EBDA at `0x9fc00`, which the Phase 1 e820 map already marks reserved.
- Fixed device slots (a slot keeps its address and IRQ even when the other device is absent):

  | Device | MMIO window | IRQ (edge, via irqfd) | Kernel command-line entry added by the VMM |
  |---|---|---|---|
  | virtio-blk | `0xd000_0000`, 4 KiB | 5 | `virtio_mmio.device=4K@0xd0000000:5` |
  | virtio-net | `0xd000_1000`, 4 KiB | 6 | `virtio_mmio.device=4K@0xd0001000:6` |

- The e820 map is unchanged from Phase 1. Guest RAM is capped at 3 GiB (ends at or below `0xc000_0000`), so the MMIO windows (above RAM, below the IOAPIC at `0xfec0_0000`) are never RAM and are not listed.
- Exactly zero or one block device and zero or one network device.
- Network backend: a TAP interface that already exists on the host. The VMM opens it; it never creates it. Creating the TAP is a one-time `sudo` step done by the user with `scripts/setup-tap.sh`.
- The VMM still never requires root.

### Behavioral rules
- All Phase 1 behavioral rules still apply, with one exception: when a seccomp filter kills the VMM (`SIGSYS`, see M7), or on `SIGKILL`, the terminal cannot be restored. These signals cannot be caught, so the "always restore the terminal" rule does not apply to them.
- **Every guest-supplied address, length, index, and flag is untrusted.** A malformed descriptor chain or request is rejected (the request fails with a virtio error status, or the queue is marked broken) — it never panics, never loops forever, never reads or writes outside guest memory, and never touches the host file outside the disk image's size.
- Guest-facing parsing (avail/used ring handling, descriptor-chain walking, descriptor chain → virtio-blk request, virtio-net TX header handling) lives in pure functions over a memory trait defined in this crate, so it can be proven and fuzzed without KVM.
- Virtqueues: split virtqueues only. `VIRTIO_RING_F_INDIRECT_DESC`, `VIRTIO_RING_F_EVENT_IDX` and packed virtqueues are not offered. A descriptor with the `INDIRECT` flag set rejects its chain. A chain with more descriptors than the queue size (a loop) is rejected.
- Queue notifications (writes to `QueueNotify`) are delivered through one ioeventfd per queue (data-matched on the queue index) and handled on the event-loop thread. All other virtio-mmio register accesses are handled on the vCPU thread that made them.
- The VMM adds the `virtio_mmio.device=...` entries to the kernel command line itself; the user never types them.
- **Block device:**
  - The image size must be a non-zero multiple of 512 bytes, else exit 2. Capacity = size / 512 sectors.
  - The host file is opened read-write, or `O_RDONLY` with `--disk-ro`.
  - Supported request types: `IN`, `OUT`, `FLUSH` (`fdatasync` on the host file). Any other type, including `GET_ID`, completes with `VIRTIO_BLK_S_UNSUPP`.
  - An `OUT` request on a read-only device completes with `VIRTIO_BLK_S_IOERR` and never touches the host file, even if the guest ignores `VIRTIO_BLK_F_RO`.
- **Network device:**
  - The TAP is opened with `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`; the VMM sets `TUNSETVNETHDRSZ` to 12 and `TUNSETOFFLOAD` to 0.
  - A TX chain is rejected if it is shorter than the 12-byte virtio-net header or longer than 65,562 bytes (the header plus at most 65,550 bytes of frame), so the guest cannot make the VMM allocate unbounded buffers.
  - Before a frame is written to the TAP, the VMM overwrites its 12-byte virtio-net header with zeros; guest-supplied header fields never reach the host kernel.

### Performance bounds
- Booting with a disk, a network device, and 4 vCPUs reaches `PHASE1-INIT-OK` in **≤ 30 s** on the WSL2 development host.
- All Kani harnesses together finish in **≤ 20 minutes**.

---

## Inputs and outputs

### CLI (additions to `vmm boot`)
```
vmm boot --kernel <bzImage> [--initrd <path.cpio>] [--mem-mib <N>] [--cmdline "<str>"]
         [--vcpus <1-4>]
         [--disk <path.img>] [--disk-ro]
         [--tap <ifname>] [--guest-mac <aa:bb:cc:dd:ee:ff>]
         [--no-seccomp]
```
| Flag | Default | Notes |
|---|---|---|
| `--cmdline` | as Phase 1 | The 2047-byte limit applies to the **final** command line, including the `virtio_mmio.device=` entries the VMM adds; longer → exit 2 |
| `--vcpus` | `1` | `0` or `> 4` → exit 2 |
| `--disk` | none | file must exist, be writable (unless `--disk-ro`), and have a non-zero size that is a multiple of 512, else exit 2 naming the path |
| `--disk-ro` | off | requires `--disk`, else exit 2. Device advertises read-only; guest writes fail; host file unchanged |
| `--tap` | none | interface must exist and be openable by the current user, else exit 2 naming the interface |
| `--guest-mac` | `06:00:ac:10:00:02` | requires `--tap`, else exit 2. A malformed or multicast MAC → exit 2 |
| `--no-seccomp` | off | debugging only; prints a warning to stderr |

Exit codes are unchanged from Phase 1. A VMM killed by seccomp has no exit code; it terminates with `SIGSYS`.

### Host network setup (one-time per WSL boot, run by the user)
`sudo scripts/setup-tap.sh` runs `ip tuntap add dev vmtap0 mode tap user $SUDO_USER` (so that user can attach to the TAP without privileges), assigns the host `172.16.0.1/24`, and brings it up. It is idempotent.

### Guest conventions
- The disk appears as `/dev/vda`; the network device as `eth0`.
- Guest network address used in tests: `172.16.0.2/24`, configured by test commands (`ip addr add`, `ip link set`), not by `/init`.
- `/init` is unchanged from Phase 1, so Phase 1 tests keep working. The initramfs has no `/mnt`; tests run `mkdir -p /mnt` before mounting the disk.

### Disk image
`scripts/build-disk.sh` creates `artifacts/disk.ext4`: a 64 MiB ext4 image, built **without root** (e.g. `mkfs.ext4 -d`), containing `/hello.txt` with the single line `phase2-disk`. Tests always work on a temporary copy, never on this file.

---

## Acceptance criteria
Every criterion is pass/fail and is checked by `scripts/verify-phase-2.sh`. Tests that need built artifacts or the TAP are `#[ignore]`d by default and run with `cargo test -- --ignored`, as in Phase 1.

**TAP precondition.** `/sys/class/net/vmtap0` exists, its `owner` equals the current user's uid, and it has the address `172.16.0.1/24`. Every criterion that uses `--tap vmtap0` checks it first; if it does not hold, that check prints `SKIP (run: sudo scripts/setup-tap.sh)` and the overall result is FAIL. The verify script itself never uses sudo.

**No concurrent TAP use.** Only one VMM can attach to `vmtap0` at a time, so tests that use it never run concurrently: the verify script runs ignored tests with `--test-threads=1`.

### Always
- **AC-0.x** All Phase 1 "Always" criteria (fmt, clippy, SAFETY comments, `/dev/kvm` error) still pass.
- **AC-0.5** `scripts/verify-phase-1.sh` still passes in full (no regression), using the Phase 2 kernel.

### M4 — virtio-blk
- **AC-4.1** The built kernel's config (`artifacts/kernel.config`) has `VIRTIO_MMIO`, `VIRTIO_MMIO_CMDLINE_DEVICES`, `VIRTIO_BLK`, `EXT4_FS`, and `X86_MPPARSE` set to `y`.
- **AC-4.2** `scripts/build-disk.sh` produces `artifacts/disk.ext4` as described, without root.
- **AC-4.3** Booting with `--disk <copy>`: `cat /sys/block/vda/size` in the guest prints `131072` (64 MiB in 512-byte sectors).
- **AC-4.4** `mkdir -p /mnt && mount /dev/vda /mnt && cat /mnt/hello.txt` prints `phase2-disk`.
- **AC-4.5** Persistence: the guest writes `persist-ok` to `/mnt/p.txt`, runs `sync`, unmounts, and reboots with `reboot -f`. A second boot on the same image copy, in a **new VMM process**, reads back `persist-ok`. The host also confirms it with `debugfs -R "cat /p.txt" <copy>`.
- **AC-4.6** Integrity: the guest writes 32 MiB from `/dev/urandom` to `/mnt/big`, records its `sha256sum`, unmounts, and runs `reboot -f`. After the VMM exits, the host extracts the file with `debugfs -R "dump /big <tmpfile>" <copy>`, and its `sha256sum` equals the guest's.
- **AC-4.7** With `--disk-ro`: a guest write to the mounted disk fails with a read-only error, and the host image's `sha256sum` is identical before and after the run.
- **AC-4.8** A missing `--disk` file, a disk image whose size is zero or not a multiple of 512, or `--disk-ro` without `--disk` → exit 2 (with the path in the error where there is one).
- **AC-4.9** Unit tests: an `OUT` request on a read-only device completes with `VIRTIO_BLK_S_IOERR` and leaves the backing file unchanged; a `GET_ID` or unknown request type completes with `VIRTIO_BLK_S_UNSUPP`.

### M5 — virtio-net
Precondition: the TAP precondition above.
- **AC-5.1** The kernel config has `VIRTIO_NET=y`.
- **AC-5.2** Booting with `--tap vmtap0`: the guest's `ip link show eth0` shows MAC `06:00:ac:10:00:02`.
- **AC-5.3** After the guest configures `172.16.0.2/24` and brings `eth0` up, `ping -c 3 172.16.0.1` from the guest reports 0% packet loss.
- **AC-5.4** While the guest runs, `ping -c 3 172.16.0.2` from the host reports 0% packet loss.
- **AC-5.5** Bulk transfer: the host serves a 16 MiB random file (e.g. `python3 -m http.server --bind 172.16.0.1`); the guest downloads it with BusyBox `wget`, and the guest's `sha256sum` equals the host's.
- **AC-5.6** A non-existent `--tap` interface → exit 2 with the interface name in the error. `--guest-mac` without `--tap`, and a malformed or multicast `--guest-mac`, → exit 2.

### M6 — Multiple vCPUs
- **AC-6.1** `--vcpus 0` and `--vcpus 5` → exit 2.
- **AC-6.2** With `--vcpus 4`: `nproc` prints `4`, and `grep -c ^processor /proc/cpuinfo` prints `4`.
- **AC-6.3** With `--vcpus 4`: the `LOC` (local timer interrupts) row of `/proc/interrupts` is non-zero for all 4 CPUs.
- **AC-6.4** Stability (requires the TAP precondition): 10 consecutive boots with `--vcpus 4 --disk <copy> --tap vmtap0` all reach `PHASE1-INIT-OK` and all exit 0 on `reboot -f`.
- **AC-6.5** With `--vcpus 4`, `reboot -f` exits the VMM with code 0 within 10 s (every vCPU thread stops; nothing hangs).
- **AC-6.6** All M4 and M5 criteria pass with `--vcpus 4`.

### M7 — Hardening
- **AC-7.1** Every long-lived VMM thread (main/event loop, each vCPU, stdin reader) installs a seccomp-BPF allowlist before it handles any guest-controlled data. Filters are defined in one module, and every allowed syscall has a one-line comment saying why it's needed. A signal handler can run on any thread, so every thread's filter allows the syscalls the terminal-restoring signal handler uses (`ioctl(TCSETS)`, `write`, `exit_group`).
- **AC-7.2** `PR_SET_NO_NEW_PRIVS` is set before the first filter is installed.
- **AC-7.3** All M4, M5, and M6 criteria pass with seccomp enabled (the default).
- **AC-7.4** The filters' mismatch action is `SECCOMP_RET_KILL_PROCESS`. A hidden test-only flag makes a vCPU thread call a syscall outside its allowlist; the VMM process is killed by `SIGSYS`, and the test asserts that.
- **AC-7.5** `--no-seccomp` works and prints a warning to stderr.
- **AC-7.6** `notes/phase-2.md` lists every allowed syscall per thread type.

### M8 — Verification
- **AC-8.1** The split-virtqueue ring handling and descriptor-chain walker, the virtio-blk request parser, and the virtio-net TX header handling are pure functions over a memory trait defined in this crate, with no dependency on KVM, in their own module(s).
- **AC-8.2** `cargo kani` verifies these harnesses, all reporting `VERIFICATION:- SUCCESSFUL`, with a queue size of 4 (so chains of up to 4 descriptors), fully symbolic descriptor addresses, lengths, flags, and next-indices, a symbolic guest-memory size, and a symbolic disk capacity:
  - (a) the blk parser never panics;
  - (b) every data range of an accepted request lies inside guest memory;
  - (c) sector × 512 + length never overflows, and any request outside the disk size is rejected;
  - (d) the chain walk always ends within the queue size (at most queue-size descriptors are read), and a chain that revisits a descriptor index is rejected when the guest does not modify the descriptor table during the walk;
  - (e) ring handling never indexes outside the rings or the descriptor table: every descriptor index used is `< queue size`, and a chain containing an `INDIRECT` descriptor is rejected;
  - (f) the net TX header handling never panics, rejects chains outside the allowed length range, and the header it hands to the TAP is all zeros.
- **AC-8.3** A `cargo-fuzz` target for the blk request parser runs for 300 s (configurable in the verify script) with zero crashes. A seed corpus is committed; the verify script fuzzes a temporary copy of it, so the committed corpus never changes.
- **AC-8.4** Every bug that Kani or the fuzzer found has a regression unit test, and `notes/phase-2.md` lists them (or states that none were found).

### Phase 2 done
- **AC-P1** `scripts/verify-phase-2.sh` prints a PASS/FAIL line per criterion (including running `verify-phase-1.sh` for AC-0.5) and exits 0 only if every criterion passes.
- **AC-P2** `notes/phase-2.md` contains: a plain-language explanation of each milestone, how a virtio request travels from the guest driver to the host file or TAP and back, the seccomp allowlists, the Kani properties proven and what they do *not* cover, and at least 12 interview questions with answers.

---

## Output files
New or changed deliverable files. The plan may split modules differently, but any change to this list must be made here first. Workflow documents (`specs/phase-2/PLAN.md`, `specs/phase-2/DECISIONS.md`) are not part of this list.

```
Cargo.toml                           # new deps; [lints.rust] check-cfg for cfg(kani)
.gitignore                           # adds fuzz/target, fuzz/artifacts
configs/phase2.config                # kernel config fragment
src/main.rs                          # new CLI flags
src/lib.rs                           # new BootConfig fields; wiring of devices, vCPU threads, event loop
src/error.rs                         # new error variants and their exit codes
src/vm.rs                            # MMIO bus, device registration, SMP setup
src/vcpu.rs                          # MMIO exits, multiple vCPU threads, shutdown of all vCPUs
src/arch/x86_64/mod.rs               # MMIO window, IRQ and MP table constants
src/arch/x86_64/regs.rs              # per-vCPU CPUID (APIC IDs, topology), LAPIC LINT0/LINT1
src/arch/x86_64/mptable.rs           # MP table for SMP
src/devices/mod.rs
src/devices/pio_bus.rs               # shared between vCPU threads
src/devices/serial.rs                # shared between vCPU threads and the stdin reader
src/devices/mmio_bus.rs              # MMIO dispatch, default all-ones/ignore behavior
src/devices/virtio/mod.rs
src/devices/virtio/mmio.rs           # virtio-mmio transport registers
src/devices/virtio/queue.rs          # split virtqueue over the memory trait (Kani + fuzz)
src/devices/virtio/block.rs          # virtio-blk device (I/O against host file)
src/devices/virtio/net.rs            # virtio-net device (I/O against TAP)
src/devices/virtio/parse.rs          # pure, guest-facing parsers (Kani + fuzz target)
src/devices/virtio/proofs.rs         # #[cfg(kani)] harnesses
src/devices/tap.rs                   # open an existing TAP interface
src/event_loop.rs                    # device event handling
src/seccomp.rs                       # per-thread allowlists
scripts/build-kernel.sh              # applies configs/phase2.config, copies .config to artifacts/kernel.config; stamp includes the fragment's hash
scripts/build-disk.sh
scripts/setup-tap.sh                 # run by the user with sudo
scripts/verify-phase-2.sh
fuzz/Cargo.toml
fuzz/fuzz_targets/blk_request.rs
fuzz/corpus/blk_request/*            # seed inputs
tests/m4_virtio_blk.rs
tests/m5_virtio_net.rs
tests/m6_smp.rs
tests/m7_seccomp.rs
notes/phase-2.md
```

---

## Out of scope for Phase 2
Snapshots and restore, vsock, balloon, rate limiters, an API socket, a jailer (chroot/namespaces/cgroups), PCI, ACPI, more than one disk or NIC, hot-plug, live migration, SEV/TDX, indirect descriptors, event-index notification suppression, packed virtqueues, and network offloads. Do not add them, even partially.

---

## Known pitfalls — PLAN.md must address each one
1. The kernel only finds virtio-mmio devices through `virtio_mmio.device=<size>@<base>:<irq>` on its command line, and only if `VIRTIO_MMIO_CMDLINE_DEVICES=y`.
2. MMIO windows must not overlap guest RAM, and the e820 map must not report them as usable RAM.
3. Device interrupts go through irqfd to an IOAPIC pin. The guest acknowledges them through the virtio-mmio `InterruptACK` register; getting edge vs. level or the interrupt-status bits wrong leads to hangs or interrupt storms.
4. Descriptor-chain addresses, lengths, and `next` indices are guest-controlled. The guest can build loops, overlapping buffers, or buffers past the end of RAM.
5. Queue notifications arrive as MMIO writes to `QueueNotify`. They are handled through ioeventfd and the event loop (Q1); the plan must show how the ioeventfds are registered and how device state is shared between the vCPU threads and the event loop.
6. SMP needs: an MP table with one processor entry per vCPU plus bus/IOAPIC entries; distinct APIC IDs in each vCPU's CPUID; LAPIC LINT0/LINT1 configured on each vCPU.
7. Shutdown with several vCPUs: when one vCPU sees reset/shutdown, the others must be stopped and joined, or the VMM hangs.
8. TAP: the `IFF_NO_PI` and `IFF_VNET_HDR` choices must match the virtio-net header size the device offers (12 bytes with `VIRTIO_F_VERSION_1`). The kernel's default vnet header size is 10, so `TUNSETVNETHDRSZ` must be set.
9. Seccomp: Rust's standard library and the crates use syscalls that aren't obvious (`futex`, `madvise`, `sigaltstack`, …). Derive the allowlists from real traces (`strace -f`), not guesses, or the VMM will be killed at random points.
10. WSL2: check that `/dev/net/tun` exists and that the TAP survives only until the next `wsl --shutdown`.
11. Only vCPU 0 (the BSP) gets the boot registers. The other vCPUs (APs) start in the wait-for-SIPI state and are started by the guest with INIT/SIPI, which the in-kernel LAPIC handles; the VMM must not set their registers, and their `KVM_RUN` blocks until the guest starts them.
12. The CPUID topology leaves (1, 4, 0xB, 0x1F) come from the host and must be rewritten to describe one consistent topology: one socket with N cores, one thread per core, APIC ID = vCPU index.
13. `X86_MPPARSE` must be enabled in the guest kernel, or Linux ignores the MP table.
14. The fuzzer adds inputs to the corpus directory it runs on; it must run on a temporary copy so the committed seed corpus stays unchanged.
15. `fuzz/` must be its own Cargo workspace, not a member of the root package, or root `cargo build/test/clippy` would try to build it with the stable toolchain.

## Resolved questions
- Q1. Queue notifications on the vCPU thread, or ioeventfd + event loop? → **ioeventfd + event loop.** Net RX needs an event loop anyway (waiting for the TAP to become readable); a notification then causes no exit to user space; vCPU threads never block on disk or TAP I/O and never need `pread`/`pwrite`/`readv`/`writev`, so their seccomp filter stays small. One ioeventfd per queue, data-matched on the queue index written to `QueueNotify` (blk queue 0; net RX 0 and TX 1). Other register accesses stay on the vCPU threads and share device state with the event loop behind a `Mutex`.
- Q2. Use the `virtio-queue` crate's split-virtqueue code, or write our own? → **Our own minimal split virtqueue** (no indirect descriptors, no event index, no packed rings) over a memory trait defined in this crate, as Firecracker does. The crate's chain iterator stops after a fixed number of steps instead of rejecting a looping chain, so it cannot meet AC-8.2(d), and its types depend on `vm-memory`'s `GuestMemory`, which Kani cannot run on. The Kani proofs cover everything the guest controls: ring indices, the descriptor-chain walk, the blk request parser, and the net TX header handling. `virtio-queue` is removed from the allowed crates.
- Q3. Which virtio feature bits do blk and net offer? → blk: `VIRTIO_F_VERSION_1`, `VIRTIO_BLK_F_FLUSH` (flush → `fdatasync`), and `VIRTIO_BLK_F_RO` with `--disk-ro`. net: `VIRTIO_F_VERSION_1`, `VIRTIO_NET_F_MAC`; no `STATUS`, no `MRG_RXBUF`, no checksum or segmentation offloads. The TAP is opened with `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`, `TUNSETVNETHDRSZ` 12, `TUNSETOFFLOAD` 0, and the VMM zeroes the 12-byte header of every TX frame.
- Q4. MMIO base addresses and IRQ numbers? → blk `0xd000_0000` (4 KiB), IRQ 5; net `0xd000_1000` (4 KiB), IRQ 6; fixed slots; edge-triggered through irqfd. Above the 3 GiB RAM cap, below the IOAPIC at `0xfec0_0000`. MP table in the EBDA at `0x9fc00` (already reserved in e820).
- Q5. What Kani bounds keep all harnesses under 20 minutes while still meaning something? → Queue size 4 (chains of at most 4 descriptors, loop unwinding 5). Guest memory is modelled as a symbolic size, with every in-bounds read returning arbitrary bytes, so every address and descriptor field stays symbolic without a byte array. The disk capacity is symbolic. If the harnesses exceed 20 minutes, reduce the queue size to 2 before cutting anything else.
- Q6. How does the verify script run the network checks without sudo? → `setup-tap.sh` creates `vmtap0` with `user $SUDO_USER`, so that user can attach to it unprivileged. The verify script checks the TAP precondition (the interface exists, its `owner` equals `id -u`, the address is set) and prints SKIP / overall FAIL otherwise. Host-side `ping` and `python3 -m http.server` need no root. Tests that use the TAP run with `--test-threads=1`.
