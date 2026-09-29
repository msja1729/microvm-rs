# Phase 2 PLAN — A real VMM (M4–M8)

Implementation plan for `specs/phase-2/SPEC.md`. It needs approval before any code is written. Each milestone is implemented, verified and committed on its own, and the next one starts only after you approve it.

Choices made inside the spec are numbered **D1–D18** and logged in `specs/phase-2/DECISIONS.md` (question, options, choice, why). This plan only refers to them.

Host facts checked (read-only): Intel i9-14900HX, 4 host CPUs visible to WSL2; `/dev/net/tun` is `crw-rw-rw-`; `ping` has `cap_net_raw`, so host-side pings need no root; `mke2fs 1.47.0` (supports `-d`); `debugfs`, `strace`, `python3`, `ip` present; `merge_config.sh` exists in the unpacked 6.12.111 tree; only the stable Rust toolchain is installed (nightly, `cargo-fuzz` and Kani come in M8). Ubuntu's `busybox-static` has every applet the tests use (`nproc wget ping ip sha256sum mount umount sync dd head reboot`).

---

## 1. Architecture

### Threads (D1)
| Thread | Created | Does | Seccomp profile (M7) |
|---|---|---|---|
| main | process start | setup; then runs the **event loop** (queue ioeventfds, TAP, exit eventfd); at exit stops and joins the vCPUs | `main` |
| `vcpu0`…`vcpu{N-1}` | after all setup | `KVM_RUN` loop; port-I/O and MMIO exits; virtio-mmio register accesses | `vcpu` |
| `stdin` | as in Phase 1 | host stdin → UART RX FIFO | `stdin` |

- A vCPU that sees a reset request, `KVM_EXIT_SHUTDOWN`, or an error stores the outcome in a shared `Mutex<Option<Outcome>>` (first writer wins) and writes the **exit eventfd**. The event loop sees it and stops.
- Stopping the other vCPUs (pitfall 7, D2): set a shared `stop` flag, then send `SIGRTMIN` to every vCPU thread that hasn't finished, every 10 ms for up to 5 s. A vCPU loop checks `stop` before each `KVM_RUN` and treats `EINTR` as "re-check `stop`". A no-op `SIGRTMIN` handler (without `SA_RESTART`) is installed once, before any vCPU thread starts. Resending covers the race where the signal lands just before `KVM_RUN`. Then every vCPU thread is joined. A thread still running after 5 s → internal error, exit 1.
- Outcome → exit code as in Phase 1: reset/shutdown → 0, errors → their code.

### Shared state
- `GuestMemoryMmap` is cloned into each thread (it is `Arc`-backed, `Send + Sync`).
- The port-I/O bus becomes `Arc<Mutex<PioBus>>`, shared by all vCPUs (serial and i8042 unchanged). Contention is irrelevant at this scale.
- `MmioBus` is built before the vCPUs start and never changes afterwards, so it is `Arc<MmioBus>` with no lock. Each entry is `Arc<Mutex<dyn MmioDevice>>`. Unclaimed MMIO addresses read all-ones and ignore writes, logged at `debug` (Phase 1 rule).
- Each virtio device is `Arc<Mutex<VirtioMmio<D>>>`: the vCPU threads use it for register accesses and the event loop for queue work. `InterruptStatus` is an `Arc<AtomicU32>` outside the lock, so reading or acknowledging it never waits for disk I/O.

### Memory map additions (Q4, pitfall 2)
| Guest-physical | Contents |
|---|---|
| `0x9fc00` | MP floating pointer + MP configuration table (M6); ≈300 bytes of the 1 KiB EBDA, already `reserved` in e820 |
| `0xd000_0000` – `0xd000_0fff` | virtio-blk MMIO window, IRQ 5 |
| `0xd000_1000` – `0xd000_1fff` | virtio-net MMIO window, IRQ 6 |

RAM ends at ≤ `0xc000_0000` (3 GiB cap), so the windows never overlap RAM and the e820 map stays as in Phase 1. A `const` assertion in `arch/x86_64/mod.rs` ties `MMIO_BASE ≥ MAX_MEM` so a future change to the RAM cap cannot silently break this. Nothing is registered with `KVM_SET_USER_MEMORY_REGION` there, so every guest access exits to user space as `KVM_EXIT_MMIO` (except `QueueNotify` writes, which KVM routes to ioeventfds).

### virtio-mmio transport (`devices/virtio/mmio.rs`, D3)
- Register layout **version 2** only (`MagicValue 0x74726976`, `Version 2`, `VendorID` `0x554d4551`). `DeviceID` 2 = blk, 1 = net.
- Registers are 32-bit and 4-byte aligned. Any other width or alignment below `0x100` reads all-ones and ignores writes (debug log). Config space (`0x100+`) accepts 1/2/4/8-byte accesses inside the device's config length; outside it reads all-ones.
- 64-bit features through `DeviceFeaturesSel`/`DriverFeaturesSel` (0 = low word, 1 = high word, other values read 0 and ignore writes).
- `Status`:
  - writing 0 resets the device (features, queues, interrupt status, activation);
  - `FEATURES_OK` is only kept if the driver's features ⊆ the device's features and include `VIRTIO_F_VERSION_1`;
  - `DRIVER_OK` activates the device after checking every enabled queue: size a power of two in 1..=256, ring addresses aligned (desc 16, avail 2, used 4) and fully inside guest RAM. On failure it sets `DEVICE_NEEDS_RESET` (0x40) and raises a config-change interrupt.
- Queue registers (`QueueSel`, `QueueNumMax` = 256 (D4), `QueueNum`, `QueueReady`, desc/driver/device address low/high) are writable only while that queue is not ready. Writes that break this rule are ignored.
- `ConfigGeneration` is always 0 (config never changes after boot).
- `QueueNotify` normally never reaches user space: KVM delivers it to the queue's ioeventfd. A write with a non-matching value does exit and is ignored.

### Interrupts (pitfall 3)
- Each device has an `EventFd` registered with `KVM_IRQFD` on its GSI (5 or 6). With KVM's default routing, GSIs 0–15 go to both PIC and IOAPIC, so this works in M4/M5 (PIC path, no MP table yet) and after M6 (IOAPIC path).
- Delivery is **edge-triggered**: the MP table's I/O interrupt entries for ISA IRQs use "conforming" polarity/trigger, i.e. edge/active-high, which is what an irqfd pulse is.
- Order after adding used buffers: write the used element(s) → `Release` fence → write `used.idx` → `InterruptStatus |= 1` (used-buffer bit) → write the irqfd. Linux's `vm_interrupt` reads `InterruptStatus`, writes the same bits to `InterruptACK` (which clears them), then scans the used ring. The status bit is set before the edge, so no notification is lost, and an edge doesn't re-fire until we write again, so there's no interrupt storm.
- Bit 1 (config change) is used only when setting `DEVICE_NEEDS_RESET`.
- The `VIRTQ_AVAIL_F_NO_INTERRUPT` hint is ignored: we always interrupt after using buffers (D4). This is allowed by the virtio spec and avoids a lost-wakeup class of bugs.

### Queue notifications (Q1, pitfall 5)
- One `EventFd` per queue, registered before any vCPU runs with `KVM_IOEVENTFD` at `base + 0x50` (`QueueNotify`), 4 bytes, data-match = queue index (blk: 0; net: RX 0, TX 1).
- The event loop (`event-manager`, D15) has one subscriber per device, which holds the same `Arc<Mutex<VirtioMmio<D>>>`. On an event it reads the eventfd, takes the lock and, if the device is active, processes the queue. Notifications before `DRIVER_OK` are drained and ignored.

### Split virtqueue and memory trait (`devices/virtio/queue.rs`, Q2, pitfall 4, D16)
- `trait GuestMem { fn size(&self) -> u64; fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), MemError>; fn write(&self, addr: u64, buf: &[u8]) -> Result<(), MemError>; }`. Bounds are checked with `checked_add` against `size()`.
  - Production: implemented for `GuestMemoryMmap` (RAM is one region starting at 0).
  - Kani: `KaniMem { size: u64 }` — symbolic size, every in-bounds read returns `kani::any()` bytes.
  - Fuzzing: `VecMem(Vec<u8>)`.
- Every guest structure is read **once** into a local copy before it is checked and used, so a guest changing a descriptor concurrently can't bypass a check (time-of-check/time-of-use).
- `Queue::pop(&mut self, mem) -> Result<Option<u16>, QueueError>`:
  - reads `avail.idx` (then an `Acquire` fence);
  - `avail.idx − next_avail` (wrapping) > size → `QueueError::Broken`;
  - otherwise reads `avail.ring[next_avail % size]`; a head index ≥ size → `Broken`.
- `walk_chain(mem, desc_table, size, head) -> Result<Chain, ChainError>`:
  - follows `next` while `F_NEXT` is set;
  - rejects an index ≥ size, the `INDIRECT` flag, and a chain longer than `size` descriptors (a loop), so it runs at most `size` steps;
  - returns up to `size` `(addr, len, writable)` entries.
- `Queue::add_used(mem, head, len)`: writes the used element at `next_used % size`, then a `Release` fence, then `used.idx`.
- What happens on errors (D5):
  - `ChainError` → the device completes the head with used length 0. The blk device also writes `VIRTIO_BLK_S_IOERR` if the last descriptor is a device-writable status byte inside RAM.
  - `QueueError::Broken` (the ring itself is corrupt) → the queue stops, and the device sets `DEVICE_NEEDS_RESET` and raises a config-change interrupt. This is "the queue is marked broken".

### virtio-blk (`parse.rs` + `block.rs`)
- Config space: `capacity` (u64, sectors) at offset 0.
- Features: `VERSION_1 | FLUSH`, plus `RO` with `--disk-ro` (Q3).
- `parse_blk_request(chain, mem, capacity_sectors) -> Result<BlkRequest, BlkError>` (pure):
  - Descriptor 0 is device-readable with len ≥ 16; it holds `type: u32, reserved: u32, sector: u64` (read once).
  - The last descriptor is device-writable with len ≥ 1; it is the status byte.
  - The descriptors in between are the data buffers: device-writable for `IN`, device-readable for `OUT`, none for `FLUSH`. Their total length is summed in `u64` with `checked_add`.
  - `IN` and `OUT`: total length is a multiple of 512, and `sector.checked_mul(512)?.checked_add(total)? ≤ capacity × 512`. Otherwise `OutOfRange` → `IOERR`.
  - Every data range satisfies `addr.checked_add(len)? ≤ mem.size()`.
  - `GET_ID` and unknown types → `Unsupported` → `VIRTIO_BLK_S_UNSUPP`.
- Executing a request (`block.rs`):
  - `OUT` on a read-only device → `IOERR` without touching the file. This check is in the device, independent of what the guest negotiated.
  - Data is copied between guest memory and the file with `FileExt::read_at`/`write_at` through one reused **64 KiB bounce buffer**, chunk by chunk (D6). A single descriptor can be up to 4 GiB, so there's no per-request allocation sized by the guest, and no `unsafe`.
  - `FLUSH` → `File::sync_data()`.
  - Any host I/O error → `IOERR`.
  - The used length is the bytes written into device-writable buffers (data for `IN`, plus 1 for the status byte).
- Disk opening (`lib.rs`): `OpenOptions` read-write (or read-only with `--disk-ro`); the file must be a regular file with a non-zero size that is a multiple of 512. Failures → `Error::Disk { path, reason }` → exit 2.

### virtio-net (`tap.rs` + `parse.rs` + `net.rs`)
- Opening the TAP (`tap.rs`, pitfall 8, pitfall 10, D17):
  1. The name must be shorter than `IFNAMSIZ`, and `/sys/class/net/<name>` must exist. Otherwise exit 2 naming the interface. Without `CAP_NET_ADMIN`, `TUNSETIFF` on a missing name fails with `EPERM`, which would give a misleading message.
  2. `/dev/net/tun` must exist. If it doesn't, exit 2 with a WSL2 hint.
  3. `open("/dev/net/tun", O_RDWR | O_NONBLOCK | O_CLOEXEC)`.
  4. `TUNSETIFF` with `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`. `EPERM`/`EBUSY` → exit 2 naming the interface ("not owned by you or already in use").
  5. `TUNSETVNETHDRSZ` = 12 (the kernel default is 10).
  6. `TUNSETOFFLOAD` = 0.
  Each ioctl gets a `SAFETY` comment. The ioctl numbers are defined with `vmm-sys-util`'s `ioctl_iow_nr!`.
- Config space: `mac[6]`. Features: `VERSION_1 | MAC` (Q3).
- TX (`parse_net_tx(chain, mem) -> Result<TxFrame, NetError>`, pure):
  - all descriptors are device-readable;
  - the total length is in `[12, 65_562]` (checked sum);
  - every range is inside RAM.
  - The frame is gathered into a reused 65,562-byte buffer, `zero_vnet_header(&mut buf[..12])` runs, and it goes out in one `write` to the TAP (one write = one frame).
  - On `EAGAIN` the frame is dropped (D8). Ethernet may drop frames, and the TX queue must not stall.
  - Rejected chains get used length 0. Every chain is completed with used length 0, which is correct for TX.
- RX (D7):
  - when the TAP is readable and the RX queue has a chain, `read` one frame (header + packet) from the TAP into the reused buffer, then scatter it into the chain's device-writable descriptors;
  - if it doesn't fit, the frame is dropped and the chain is completed with length 0 (with MTU 1500 and Linux's 1,518-byte + header small buffers, this doesn't happen in practice);
  - the chain is completed with the number of bytes written.
  - When the RX queue has no buffers, the TAP's interest in the event loop is switched off, because epoll is level-triggered and would otherwise spin. It is switched back on when the RX queue is notified.
- `--guest-mac` is parsed in `main.rs` by a clap value parser: exactly six `:`-separated hex octets, and a multicast MAC (bit 0 of the first octet set) is rejected → exit 2.

### Command line (pitfall 1)
The final command line is the user's (or default) command line, then ` virtio_mmio.device=4K@0xd0000000:5` if a disk is configured, then ` virtio_mmio.device=4K@0xd0001000:6` if a TAP is configured. The 2047-byte limit is checked on the final string (exit 2). The kernel must have `VIRTIO_MMIO_CMDLINE_DEVICES=y` (AC-4.1).

### SMP (M6; pitfalls 6, 11, 12, 13)
- **MP table** (`arch/x86_64/mptable.rs`, D9), built as a little-endian byte vector with no `unsafe` and written at `0x9fc00`, which Linux scans as the "last KiB of base memory". Contents:
  - floating pointer `_MP_` pointing to the config table (spec rev 4);
  - config header `PCMP` (spec rev 4, LAPIC address `0xfee0_0000`);
  - one processor entry per vCPU (APIC ID = index, LAPIC version `0x14`, enabled, BSP flag on vCPU 0, CPU signature and feature flags from CPUID leaf 1);
  - one bus entry `ISA`;
  - one IOAPIC entry (ID = N, version `0x11`, enabled, address `0xfec0_0000`);
  - I/O interrupt entries for ISA IRQs 0–15 → IOAPIC pins 0–15, conforming polarity and trigger;
  - local interrupt entries ExtINT → LINT0 and NMI → LINT1 for all LAPICs.
  Both checksums make the byte sum 0. A unit test re-parses the bytes and checks both checksums and the entry counts.
- **Written in every `boot`**, including `--vcpus 1`.
- **X86_MPPARSE**: in `configs/phase2.config` and checked by `build-kernel.sh` and AC-4.1. Without it Linux ignores the table (pitfall 13).
- **Per-vCPU CPUID** (`regs.rs`, pitfall 12, D10): start from `KVM_GET_SUPPORTED_CPUID`, then rewrite for "1 socket, N cores, 1 thread per core, APIC ID = vCPU index":

  | Leaf | Changes |
  |---|---|
  | 1 | `EBX[31:24]` = APIC ID; `EBX[23:16]` = N; `EDX.HTT` set when N > 1 |
  | 4 (each cache subleaf) | `EAX[31:26]` = N − 1; `EAX[25:14]` = N − 1 for the shared L3 and 0 for L1/L2 |
  | 0xB / 0x1F | subleaf 0 (SMT: shift 0, 1 logical, type 1); subleaf 1 (core: shift ⌈log2 N⌉, N logical, type 2); subleaf ≥ 2 (type 0); `EDX` = x2APIC ID in every subleaf |

  The development host is Intel; AMD topology leaves are left as KVM reports them and documented as not supported in Phase 2.
- **LAPIC LINT0/LINT1** (`regs.rs`): for every vCPU, `KVM_GET_LAPIC` → LVT LINT0 (offset `0x350`) = ExtINT, LVT LINT1 (`0x360`) = NMI → `KVM_SET_LAPIC`.
- **BSP only** (pitfall 11): all N vCPUs are created in the main thread after the irqchip (Phase 1 pitfall 2). Only vCPU 0 gets `setup_long_mode` and `RSI = boot_params`. The APs keep KVM's initial `KVM_MP_STATE_UNINITIALIZED` (wait-for-SIPI). Their `KVM_RUN` blocks in the kernel until the guest sends INIT/SIPI, which the in-kernel LAPIC handles. The stop protocol above interrupts that block too (`EINTR`).

### Seccomp (M7; pitfall 9, D12)
- `seccomp.rs` builds three `seccompiler` filters in Rust code (not JSON):
  - mismatch action `KillProcess` (AC-7.4), match action `Allow`, target arch `x86_64`;
  - one line per syscall with a `// why` comment;
  - `ioctl` is argument-filtered to the request numbers each thread actually uses (vCPU: `KVM_RUN`; terminal restore: `TCSETS`; …).
- `PR_SET_NO_NEW_PRIVS` is set once in the main thread **before any thread is spawned**; it is inherited by every later thread (AC-7.2).
- Where each filter is installed:
  - each vCPU thread: first thing, before its first `KVM_RUN`;
  - `stdin` thread: before its first `read`;
  - main: right before entering the event loop, after every file, eventfd, KVM object and thread has been created.
- Every filter also allows the Phase 1 signal handler's syscalls (`ioctl(TCSETS)` via `tcsetattr`, `write`, `exit_group`), plus `rt_sigreturn` (AC-7.1).
- Allowlists come from traces (pitfall 9): run the full M4–M6 test suite plus an interactive session with `--no-seccomp` under `strace -f -qq -o trace`, group the syscalls by thread name, and use their union per thread type. Rarely-hit paths are then exercised deliberately: TAP `EAGAIN`, disk flush, the vCPU stop kicks, a signal-triggered exit. The derivation steps and the final lists go into `notes/phase-2.md` (AC-7.6).
- `--no-seccomp` prints `vmm: warning: seccomp disabled (--no-seccomp); for debugging only` to stderr with `eprintln!`, independent of `RUST_LOG`.
- Hidden flag `--seccomp-test-violation` (clap `hide = true`, like `--kvm-device`): vCPU 0 calls `socket(AF_INET, SOCK_STREAM, 0)` right after installing its filter. No VMM thread ever legitimately calls `socket`.

---

## 2. Milestones

Each milestone ends with: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, the milestone's ignored tests, the verify-script sections implemented so far, a `DECISIONS.md` update, one commit, and three interview questions for you. `verify-phase-2.sh` grows one section per milestone, following the Phase 1 pattern.

### M4 — virtio-blk
Steps:
1. **Kernel.** Add `configs/phase2.config`:
   ```
   CONFIG_VIRTIO_MMIO=y
   CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y
   CONFIG_VIRTIO_BLK=y
   CONFIG_VIRTIO_NET=y
   CONFIG_EXT4_FS=y
   CONFIG_X86_MPPARSE=y
   ```
   `build-kernel.sh`: after `kvm_guest.config`, run `scripts/kconfig/merge_config.sh -m` with the fragment, then `make olddefconfig`. Fail if any line of the fragment is missing from `.config`, copy `.config` to `artifacts/kernel.config`, and stamp `"$VERSION $(sha256sum configs/phase2.config)"`. The Phase 1 stamp no longer matches, so the next run rebuilds (pitfall 1, spec stack rule).
2. **Disk image.** `scripts/build-disk.sh`: check that `mkfs.ext4`/`debugfs` exist (else print `sudo apt install e2fsprogs`), stage a directory with `hello.txt` = `phase2-disk\n`, `truncate -s 64M`, `mkfs.ext4 -q -F -d <stage> artifacts/disk.ext4`. It rebuilds every run (under a second).
3. **Crates.** Add `virtio-bindings` and `event-manager`, and `tempfile` as a dev-dependency (already allowed by Phase 1). Check `cargo tree -d`. If `event-manager` needs a different `vmm-sys-util`, use `vmm_sys_util::epoll` directly instead (D15). Not using an allowed crate needs no spec change.
4. **CLI** (`main.rs`, `lib.rs`): `--disk`, `--disk-ro` (clap `requires = "disk"`); disk opening and validation; building the final command line.
5. **MMIO path:**
   - `mmio_bus.rs`;
   - `vcpu.rs` dispatches `MmioRead`/`MmioWrite` to it;
   - the vCPU moves to its own thread, and main becomes the event loop (N = 1 for now; M6 generalizes it);
   - the exit eventfd and the stop/kick/join protocol arrive here, because with one vCPU on its own thread the same shutdown problem already exists (D18).
6. **virtio core:** `queue.rs` (memory trait, queue, chain walker); `mmio.rs` (transport); `parse.rs` (blk parser); `block.rs`; `event_loop.rs`; the ioeventfd + irqfd registration in `vm.rs`.
7. **Unit tests** (in the modules, using `VecMem`):
   - chain walker: loop, `INDIRECT`, index ≥ size;
   - `pop` with a corrupt `avail.idx`;
   - parser: each request type, range overflow, bad direction flags, missing status;
   - device: `OUT` on a read-only device → `IOERR` with the file unchanged, and `GET_ID`/unknown → `UNSUPP` (AC-4.9);
   - transport: register reads, feature negotiation, reset.
8. **`tests/m4_virtio_blk.rs`** (rexpect, `#[ignore]`, one test per AC-4.3…4.8), working on `tempfile` copies of `disk.ext4`:
   - AC-4.5 runs two VMM processes on one copy, then `debugfs -R "cat /p.txt"`;
   - AC-4.6: guest `dd if=/dev/urandom of=/mnt/big bs=1M count=32`, `sha256sum`, `umount`, `reboot -f`; then host `debugfs -R "dump /big <tmp>"` + `sha256sum` and compare;
   - AC-4.7: host checksum before and after, guest `mkdir -p /mnt; mount /dev/vda /mnt; echo x > /mnt/y` expecting `Read-only file system`;
   - AC-4.8: exit code and stderr only, no kernel needed.
   Tests read `VMM_TEST_VCPUS` (default 1) and pass `--vcpus` when set, so M6 can rerun them with 4 vCPUs (D11). Until M6 adds `--vcpus`, the variable is ignored.
9. **`verify-phase-2.sh`** skeleton: AC-0.x, AC-0.5 (runs `build-kernel.sh`, then `verify-phase-1.sh`), AC-4.1…4.9.

### M5 — virtio-net
Steps:
1. `scripts/setup-tap.sh`:
   - it refuses to run without root or with an empty `$SUDO_USER`;
   - it checks `/dev/net/tun` (pitfall 10);
   - if `vmtap0` exists but its owner isn't `$SUDO_USER`'s uid, it fails with a message;
   - otherwise it runs `ip tuntap add dev vmtap0 mode tap user $SUDO_USER` (unless the TAP exists), then `ip addr replace 172.16.0.1/24 dev vmtap0` and `ip link set vmtap0 up`;
   - it prints that the TAP lasts until the next `wsl --shutdown`.
2. `tap.rs`, the net parts of `parse.rs`, `net.rs`, and the net subscriber in the event loop. CLI: `--tap`, and `--guest-mac` (`requires = "tap"`, MAC parser).
3. Unit tests: TX length bounds (11, 12, 65,562, 65,563 bytes), writable TX descriptor rejected, header zeroed, RX scatter across descriptors, RX frame larger than the chain dropped, MAC parser cases.
4. **`tests/m5_virtio_net.rs`** (`#[ignore]`; each test first checks the TAP precondition and fails with the SKIP message if it doesn't hold):
   - AC-5.2: `ip link show eth0` contains `06:00:ac:10:00:02`.
   - AC-5.3: `ip addr add 172.16.0.2/24 dev eth0; ip link set eth0 up; ping -c 3 172.16.0.1` → `0% packet loss`.
   - AC-5.4: after configuring the guest, the test itself runs `ping -c 3 172.16.0.2` on the host.
   - AC-5.5: the test writes a 16 MiB random file into a `tempfile` dir, starts `python3 -m http.server 8000 --bind 172.16.0.1 --directory <dir>` (killed on drop), the guest runs `wget -O /f http://172.16.0.1:8000/f && sha256sum /f`, and the test compares checksums.
   - AC-5.6: exit codes and messages.
5. `verify-phase-2.sh`: a `tap_precondition` function (`/sys/class/net/vmtap0` exists, `owner` = `id -u`, `ip -4 addr show vmtap0` has `172.16.0.1/24`); M5 checks print `SKIP (run: sudo scripts/setup-tap.sh)` and count as FAIL. All ignored tests run with `--test-threads=1`.
6. **Needs you once:** `sudo scripts/setup-tap.sh` before running the M5 checks. I can't run sudo.

### M6 — Multiple vCPUs
Steps:
1. `--vcpus` (clap range 1..=4).
2. `mptable.rs` + a unit test; written in every boot.
3. `regs.rs`: per-vCPU CPUID rewrite and LINT0/LINT1 setup, each with a unit test on the rewritten CPUID entries (no KVM needed).
4. `vcpu.rs`/`lib.rs`: create N vCPUs, boot registers on vCPU 0 only, N threads named `vcpu{i}`, the same stop/kick/join protocol for N.
5. **`tests/m6_smp.rs`**:
   - AC-6.1: exit codes;
   - AC-6.2: `nproc` and `grep -c ^processor /proc/cpuinfo` print `4`;
   - AC-6.3: parse the `LOC:` row and check all 4 counts are > 0;
   - AC-6.4: 10 sequential boots with disk + TAP, TAP precondition checked first;
   - AC-6.5: `reboot -f`, then exit 0 within 10 s.
6. `verify-phase-2.sh`: AC-6.1…6.5; AC-6.6 reruns the M4 and M5 test binaries with `VMM_TEST_VCPUS=4`. AC-0.5 now also proves the MP-table/IOAPIC switch didn't break Phase 1.

### M7 — Hardening
Steps:
1. Trace the syscalls (see §1 Seccomp) with `--no-seccomp`.
2. Add `seccompiler`; write `seccomp.rs`; add `PR_SET_NO_NEW_PRIVS` and the install points; add `--no-seccomp` and `--seccomp-test-violation`.
3. **`tests/m7_seccomp.rs`**:
   - AC-7.4: spawn with `--seccomp-test-violation` and assert `ExitStatusExt::signal() == Some(SIGSYS)`;
   - AC-7.5: `--no-seccomp` prints the warning and still boots to `PHASE1-INIT-OK`;
   - a unit test checks that each filter compiles to BPF.
4. AC-7.1/7.2 are checked in the verify script by `grep`:
   - the filters live only in `seccomp.rs`;
   - every allowed syscall line has a comment;
   - `PR_SET_NO_NEW_PRIVS` appears before the first `apply_filter` call.
   It also checks, with `/proc/<pid>/task/*/status`, that `Seccomp: 2` is set on every thread of a running VMM.
5. AC-7.3: rerun the M4, M5 and M6 checks (seccomp is on by default).
6. Start `notes/phase-2.md` with the allowlists per thread type (AC-7.6).

### M8 — Verification
Steps:
1. **Tools** (no sudo): `rustup toolchain install nightly`, `cargo install cargo-fuzz`, `cargo install --locked kani-verifier && cargo kani setup`. Kani downloads its own toolchain (about 1–2 GB). I'll report the size and time.
2. **`proofs.rs`** (`#[cfg(kani)]`, queue size 4, `#[kani::unwind(5)]`, `KaniMem` with a symbolic size, symbolic capacity):
   | Harness | Property |
   |---|---|
   | `pop_stays_in_ring` | (e): `pop` never panics; any returned head is `< size`; a corrupt `avail.idx` → `Broken` |
   | `walk_chain_bounded` | (d) + (e): terminates within `size` steps; every index `< size`; `INDIRECT` or a loop → `Err`; `≤ size` descriptors returned |
   | `blk_parse_no_panic` | (a) |
   | `blk_parse_ranges_in_memory` | (b): for `Ok(req)`, every data range and the status byte satisfy `addr + len ≤ mem.size()` (checked in `u128`) |
   | `blk_parse_sector_bounds` | (c): for `Ok(req)` of type `IN`/`OUT`, `sector × 512 + len ≤ capacity × 512` holds in `u128`, so no `u64` overflow was possible; out-of-range requests → `Err` |
   | `net_tx_bounds` | (f): `parse_net_tx` never panics; `Ok` implies total length in `[12, 65_562]` and all ranges in memory |
   | `net_tx_header_zeroed` | (f): `zero_vnet_header` on a symbolic buffer of symbolic length ≤ 64 never panics and leaves bytes 0..12 zero |
   Measure the total time. If it's over 20 minutes, the queue size goes to 2 (Q5).
3. **Fuzzing:**
   - `fuzz/` is created with its own `[workspace]` (pitfall 15) and depends on `kvm-hello` by path.
   - Target `blk_request` (D13): the input's first 8 bytes are the disk capacity in sectors, the rest is a 64 KiB `VecMem` image (zero-padded) with a fixed queue layout: queue size 16, descriptor table at 0x0, avail ring at 0x100, used ring at 0x200. It drives `pop` → `walk_chain` → `parse_blk_request` → `add_used` until the queue is empty or broken.
   - Seed corpus (D14): 5 inputs (`IN`, `OUT`, `FLUSH`, `GET_ID`, a 2-descriptor loop), generated once by a short Python snippet recorded in `notes/phase-2.md`; only the binary files are committed.
   - The verify script copies the corpus to `mktemp -d` and runs `cargo +nightly fuzz run blk_request <tmp> -- -max_total_time=${FUZZ_SECONDS:-300}` (pitfall 14).
4. **AC-8.4:** each Kani counterexample or fuzz crash becomes a `#[test]` in `queue.rs`/`parse.rs` named `regression_*`, and is listed in the notes (or the notes say none were found).
5. **`verify-phase-2.sh`:**
   - AC-8.1: `grep` that `queue.rs` and `parse.rs` don't import `kvm_ioctls`/`kvm_bindings`;
   - AC-8.2: `timeout 1200 cargo kani`, and every harness reports `VERIFICATION:- SUCCESSFUL`;
   - AC-8.3: the fuzz run;
   - AC-8.4 and AC-P2: `grep`-based note checks, as in Phase 1;
   - AC-P1.
6. **Finish `notes/phase-2.md`** (AC-P2): the milestones in plain language; the request path guest driver → avail ring → ioeventfd → event loop → host file/TAP → used ring → irqfd → guest; the allowlists; the Kani properties and their limits (only queue size 4; `KaniMem` models memory as arbitrary bytes, not real contents; the device I/O code and the transport registers aren't proven; the fuzzer covers blk only); and ≥ 12 interview questions with answers.

---

## 3. Known pitfalls → where they are handled
| # | Pitfall | Handled by |
|---|---|---|
| 1 | Kernel needs `virtio_mmio.device=` and `VIRTIO_MMIO_CMDLINE_DEVICES` | §1 Command line; M4 step 1; AC-4.1 |
| 2 | MMIO windows vs RAM and e820 | §1 Memory map; `const` assertion; e820 unchanged |
| 3 | irqfd/IOAPIC, `InterruptACK`, edge vs level | §1 Interrupts (edge irqfd, status-before-edge ordering, ACK clears bits) |
| 4 | Guest-controlled addresses, lengths, `next`, loops, overlaps, OOB | §1 Split virtqueue (read-once copies, bounded walk, checked ranges); Kani (b)(d)(e); fuzzing |
| 5 | `QueueNotify` handling and state sharing | §1 Queue notifications (ioeventfd + event loop; `Arc<Mutex<VirtioMmio>>` + atomic interrupt status) |
| 6 | MP table, distinct APIC IDs, LINT0/LINT1 | §1 SMP; M6 steps 2–3 |
| 7 | Stopping and joining every vCPU | §1 Threads (exit eventfd, stop flag, repeated `SIGRTMIN` kicks, 5 s join deadline); AC-6.5 |
| 8 | `IFF_NO_PI`/`IFF_VNET_HDR` vs 12-byte header | §1 virtio-net (`TUNSETVNETHDRSZ` 12, `TUNSETOFFLOAD` 0) |
| 9 | Non-obvious syscalls | §1 Seccomp (allowlists from `strace -f` traces, rare paths exercised on purpose) |
| 10 | WSL2 `/dev/net/tun`, TAP lifetime | `setup-tap.sh` and `tap.rs` check `/dev/net/tun`; script and notes state the `wsl --shutdown` lifetime |
| 11 | BSP-only boot registers; APs wait for SIPI | §1 SMP (BSP only) |
| 12 | Consistent CPUID topology | §1 SMP (leaf 1/4/0xB/0x1F rewrite) |
| 13 | `X86_MPPARSE` | `configs/phase2.config`; `build-kernel.sh` check; AC-4.1 |
| 14 | Fuzzer grows the committed corpus | M8 step 3 (temporary corpus copy) |
| 15 | `fuzz/` outside the root workspace | M8 step 3 (`[workspace]` in `fuzz/Cargo.toml`) |

## 4. Risks
- **Crate compatibility.** `event-manager 0.4.2` and `seccompiler 0.5` may pin a different `vmm-sys-util`/`libc` than Phase 1. Fallback for the event loop: `vmm_sys_util::epoll` (D15). If `seccompiler` conflicts, I'll stop and ask; it has no allowed substitute.
- **Kernel rebuild time.** The config change forces a full rebuild in M4 (about as long as the Phase 1 build).
- **Nested SMP.** The host has 4 CPUs for 4 vCPUs under nested virtualization. The 30 s boot bound and AC-6.4's 10 boots may be slow but should pass. If the boot bound fails for this reason, I'll stop and propose a spec change rather than tune around it.
- **Kani cost.** The download size and harness time are unknown until M8. Q5 gives the fallback.

## 5. Needs from you
- Approval of this plan before M4 starts.
- M5: running `sudo scripts/setup-tap.sh` once (and again after each `wsl --shutdown`).
