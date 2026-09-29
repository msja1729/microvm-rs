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
| M8 — Verification | Kani proofs and a fuzz target show the guest-facing request parsers cannot panic, overflow, loop forever, or access memory out of bounds |

---

## Constraints

### Stack
- Everything in the Phase 1 spec's Stack section still applies.
- Newly allowed runtime crates: `virtio-queue`, `virtio-bindings`, `seccompiler`, `event-manager`.
- Newly allowed tools (installed without sudo): a nightly Rust toolchain (only for fuzzing), `cargo-fuzz`, and Kani (`cargo install --locked kani-verifier && cargo kani setup`). The fuzz target lives in a separate `fuzz/` crate that may depend on `libfuzzer-sys`.
- Any other crate requires updating this spec first.
- Guest kernel: still Linux 6.12 LTS, built from `x86_64_defconfig` + `kvm_guest.config` + a committed fragment `configs/phase2.config` that enables at least `VIRTIO_MMIO`, `VIRTIO_MMIO_CMDLINE_DEVICES`, `VIRTIO_BLK`, `VIRTIO_NET`, and `EXT4_FS` as built-in (`=y`).

### Architecture limits (Phase 2 only)
- Device transport: virtio-mmio only. Still no PCI and no ACPI.
- Up to 4 vCPUs. SMP is described to the guest with an MP table (not ACPI).
- Exactly zero or one block device and zero or one network device.
- Network backend: a TAP interface that already exists on the host. The VMM opens it; it never creates it. Creating the TAP is a one-time `sudo` step done by the user with `scripts/setup-tap.sh`.
- The VMM still never requires root.

### Behavioral rules
- All Phase 1 behavioral rules still apply.
- **Every guest-supplied address, length, index, and flag is untrusted.** A malformed descriptor chain or request is rejected (the request fails with a virtio error status, or the queue is marked broken) — it never panics, never loops forever, never reads or writes outside guest memory, and never touches the host file outside the disk image's size.
- Guest-facing parsing (descriptor chain → virtio-blk request; virtio-net header handling) lives in pure functions that take a memory abstraction, so they can be proven and fuzzed without KVM.
- The VMM adds the `virtio_mmio.device=...` entries to the kernel command line itself; the user never types them.

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
| `--vcpus` | `1` | `0` or `> 4` → exit 2 |
| `--disk` | none | file must exist and be writable (unless `--disk-ro`), else exit 2 |
| `--disk-ro` | off | device advertises read-only; guest writes fail; host file unchanged |
| `--tap` | none | interface must exist and be openable by the current user, else exit 2 naming the interface |
| `--guest-mac` | `06:00:ac:10:00:02` | |
| `--no-seccomp` | off | debugging only; prints a warning to stderr |

Exit codes are unchanged from Phase 1.

### Host network setup (one-time per WSL boot, run by the user)
`sudo scripts/setup-tap.sh` creates TAP `vmtap0` owned by the invoking user, assigns the host `172.16.0.1/24`, and brings it up. It is idempotent.

### Guest conventions
- The disk appears as `/dev/vda`; the network device as `eth0`.
- Guest network address used in tests: `172.16.0.2/24`, configured by test commands (`ip addr add`, `ip link set`), not by `/init`.
- `/init` is unchanged from Phase 1, so Phase 1 tests keep working.

### Disk image
`scripts/build-disk.sh` creates `artifacts/disk.ext4`: a 64 MiB ext4 image, built **without root** (e.g. `mkfs.ext4 -d`), containing `/hello.txt` with the single line `phase2-disk`. Tests always work on a temporary copy, never on this file.

---

## Acceptance criteria
Every criterion is pass/fail and is checked by `scripts/verify-phase-2.sh`. Tests that need built artifacts or the TAP are `#[ignore]`d by default and run with `cargo test -- --ignored`, as in Phase 1.

### Always
- **AC-0.x** All Phase 1 "Always" criteria (fmt, clippy, SAFETY comments, `/dev/kvm` error) still pass.
- **AC-0.5** `scripts/verify-phase-1.sh` still passes in full (no regression), using the Phase 2 kernel.

### M4 — virtio-blk
- **AC-4.1** The built kernel's config (`artifacts/kernel.config`) has `VIRTIO_MMIO`, `VIRTIO_MMIO_CMDLINE_DEVICES`, `VIRTIO_BLK`, and `EXT4_FS` set to `y`.
- **AC-4.2** `scripts/build-disk.sh` produces `artifacts/disk.ext4` as described, without root.
- **AC-4.3** Booting with `--disk <copy>`: `cat /sys/block/vda/size` in the guest prints `131072` (64 MiB in 512-byte sectors).
- **AC-4.4** `mount /dev/vda /mnt && cat /mnt/hello.txt` prints `phase2-disk`.
- **AC-4.5** Persistence: the guest writes `persist-ok` to `/mnt/p.txt`, runs `sync`, unmounts, and reboots with `reboot -f`. A second boot on the same image copy reads back `persist-ok`. The host also confirms it with `debugfs -R "cat /p.txt" <copy>`.
- **AC-4.6** Integrity: the guest writes 32 MiB from `/dev/urandom` to `/mnt/big`, records its `sha256sum`, unmounts, remounts, and the checksum matches.
- **AC-4.7** With `--disk-ro`: a guest write to the mounted disk fails with a read-only error, and the host image's `sha256sum` is identical before and after the run.
- **AC-4.8** A missing `--disk` file → exit 2 with the path in the error.

### M5 — virtio-net
Precondition: `sudo scripts/setup-tap.sh` has been run. If `vmtap0` is missing, the M5 checks print `SKIP (run: sudo scripts/setup-tap.sh)` and the overall result is FAIL.
- **AC-5.1** The kernel config has `VIRTIO_NET=y`.
- **AC-5.2** Booting with `--tap vmtap0`: the guest's `ip link show eth0` shows MAC `06:00:ac:10:00:02`.
- **AC-5.3** After the guest configures `172.16.0.2/24` and brings `eth0` up, `ping -c 3 172.16.0.1` from the guest reports 0% packet loss.
- **AC-5.4** While the guest runs, `ping -c 3 172.16.0.2` from the host reports 0% packet loss.
- **AC-5.5** Bulk transfer: the host serves a 16 MiB random file (e.g. `python3 -m http.server --bind 172.16.0.1`); the guest downloads it with BusyBox `wget`, and the guest's `sha256sum` equals the host's.
- **AC-5.6** A non-existent `--tap` interface → exit 2 with the interface name in the error.

### M6 — Multiple vCPUs
- **AC-6.1** `--vcpus 0` and `--vcpus 5` → exit 2.
- **AC-6.2** With `--vcpus 4`: `nproc` prints `4`, and `grep -c ^processor /proc/cpuinfo` prints `4`.
- **AC-6.3** With `--vcpus 4`: the `LOC` (local timer interrupts) row of `/proc/interrupts` is non-zero for all 4 CPUs.
- **AC-6.4** Stability: 10 consecutive boots with `--vcpus 4 --disk <copy> --tap vmtap0` all reach `PHASE1-INIT-OK` and all exit 0 on `reboot -f`.
- **AC-6.5** With `--vcpus 4`, `reboot -f` exits the VMM with code 0 within 10 s (every vCPU thread stops; nothing hangs).
- **AC-6.6** All M4 and M5 criteria pass with `--vcpus 4`.

### M7 — Hardening
- **AC-7.1** Every long-lived VMM thread (main/event loop, each vCPU, stdin reader) installs a seccomp-BPF allowlist before it handles any guest-controlled data. Filters are defined in one module, and every allowed syscall has a one-line comment saying why it's needed.
- **AC-7.2** `PR_SET_NO_NEW_PRIVS` is set before the first filter is installed.
- **AC-7.3** All M4, M5, and M6 criteria pass with seccomp enabled (the default).
- **AC-7.4** A hidden test-only flag makes a vCPU thread call a syscall outside its allowlist; the VMM process is killed by `SIGSYS`, and the test asserts that.
- **AC-7.5** `--no-seccomp` works and prints a warning to stderr.
- **AC-7.6** `notes/phase-2.md` lists every allowed syscall per thread type.

### M8 — Verification
- **AC-8.1** The virtio-blk request parser and the virtio-net header handling are pure functions over a memory trait, with no dependency on KVM, in their own module(s).
- **AC-8.2** `cargo kani` verifies these harnesses, all reporting `VERIFICATION:- SUCCESSFUL`, for descriptor chains of up to 4 descriptors with fully symbolic addresses, lengths, flags, and next-indices:
  - (a) the blk parser never panics;
  - (b) every data range of an accepted request lies inside guest memory;
  - (c) sector × 512 + length never overflows, and any request outside the disk size is rejected;
  - (d) a chain that loops back on itself is rejected in a bounded number of steps.
- **AC-8.3** A `cargo-fuzz` target for the blk request parser runs for 300 s (configurable in the verify script) with zero crashes. A seed corpus is committed.
- **AC-8.4** Every bug that Kani or the fuzzer found has a regression unit test, and `notes/phase-2.md` lists them (or states that none were found).

### Phase 2 done
- **AC-P1** `scripts/verify-phase-2.sh` prints a PASS/FAIL line per criterion (including running `verify-phase-1.sh` for AC-0.5) and exits 0 only if every criterion passes.
- **AC-P2** `notes/phase-2.md` contains: a plain-language explanation of each milestone, how a virtio request travels from the guest driver to the host file or TAP and back, the seccomp allowlists, the Kani properties proven and what they do *not* cover, and at least 12 interview questions with answers.

---

## Output files
New or changed files. The plan may split modules differently, but any change to this list must be made here first.

```
Cargo.toml                           # new deps
configs/phase2.config                # kernel config fragment
src/main.rs                          # new CLI flags
src/vm.rs                            # MMIO bus, device registration, SMP setup
src/vcpu.rs                          # MMIO exits, multiple vCPU threads, shutdown of all vCPUs
src/arch/x86_64/mptable.rs           # MP table for SMP
src/arch/x86_64/boot_params.rs       # e820 must exclude MMIO windows
src/devices/mmio_bus.rs              # MMIO dispatch, default all-ones/ignore behavior
src/devices/virtio/mod.rs
src/devices/virtio/mmio.rs           # virtio-mmio transport registers
src/devices/virtio/block.rs          # virtio-blk device (I/O against host file)
src/devices/virtio/net.rs            # virtio-net device (I/O against TAP)
src/devices/virtio/parse.rs          # pure, guest-facing parsers (Kani + fuzz target)
src/devices/virtio/proofs.rs         # #[cfg(kani)] harnesses
src/devices/tap.rs                   # open an existing TAP interface
src/event_loop.rs                    # device event handling
src/seccomp.rs                       # per-thread allowlists
scripts/build-kernel.sh              # applies configs/phase2.config, copies .config to artifacts/kernel.config
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
specs/phase-2/PLAN.md
specs/phase-2/DECISIONS.md
```

---

## Out of scope for Phase 2
Snapshots and restore, vsock, balloon, rate limiters, an API socket, a jailer (chroot/namespaces/cgroups), PCI, ACPI, more than one disk or NIC, hot-plug, live migration, SEV/TDX. Do not add them, even partially.

---

## Known pitfalls — PLAN.md must address each one
1. The kernel only finds virtio-mmio devices through `virtio_mmio.device=<size>@<base>:<irq>` on its command line, and only if `VIRTIO_MMIO_CMDLINE_DEVICES=y`.
2. MMIO windows must not overlap guest RAM, and the e820 map must not report them as usable RAM.
3. Device interrupts go through irqfd to an IOAPIC pin. The guest acknowledges them through the virtio-mmio `InterruptACK` register; getting edge vs. level or the interrupt-status bits wrong leads to hangs or interrupt storms.
4. Descriptor-chain addresses, lengths, and `next` indices are guest-controlled. The guest can build loops, overlapping buffers, or buffers past the end of RAM.
5. Queue notifications arrive as MMIO writes to `QueueNotify`. The plan must decide whether they are handled on the vCPU thread or through ioeventfd and the event loop, and justify it.
6. SMP needs: an MP table with one processor entry per vCPU plus bus/IOAPIC entries; distinct APIC IDs in each vCPU's CPUID; LAPIC LINT0/LINT1 configured on each vCPU.
7. Shutdown with several vCPUs: when one vCPU sees reset/shutdown, the others must be stopped and joined, or the VMM hangs.
8. TAP: the `IFF_NO_PI` and `IFF_VNET_HDR` choices must match the virtio-net header size the device offers (12 bytes with `VIRTIO_F_VERSION_1`).
9. Seccomp: Rust's standard library and the crates use syscalls that aren't obvious (`futex`, `madvise`, `sigaltstack`, …). Derive the allowlists from real traces (`strace -f`), not guesses, or the VMM will be killed at random points.
10. WSL2: check that `/dev/net/tun` exists and that the TAP survives only until the next `wsl --shutdown`.

## Open questions — resolve during planning, then update this spec
- Q1. Queue notifications on the vCPU thread, or ioeventfd + event loop?
- Q2. Use the `virtio-queue` crate's split-virtqueue code, or write our own? If the crate is used, what exactly do the Kani proofs cover?
- Q3. Which virtio feature bits do blk and net offer (e.g. `VIRTIO_BLK_F_RO`, `VIRTIO_BLK_F_FLUSH`, `VIRTIO_NET_F_MAC`)? No offloads in Phase 2.
- Q4. MMIO base addresses and IRQ numbers for each device.
- Q5. What Kani bounds (number of descriptors, memory size) keep all harnesses under 20 minutes while still meaning something?
- Q6. How does the verify script run the network checks, which need the TAP to exist, without needing sudo itself?