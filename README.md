# microvm-rs

A minimal, Firecracker-style **virtual machine monitor** written in Rust on top of Linux KVM
and the [rust-vmm](https://github.com/rust-vmm) crates. It boots an unmodified Linux 6.12
kernel to a BusyBox shell with a serial console, virtio-mmio block and network devices, up to
four vCPUs, and per-thread seccomp filters. It runs as a normal user, never as root.

```
$ vmm boot --kernel bzImage --initrd initramfs.cpio --disk disk.ext4 --vcpus 4
...
PHASE1-INIT-OK
phase1# nproc
4
phase1# mkdir -p /mnt && mount /dev/vda /mnt && cat /mnt/hello.txt
phase2-disk
```

## Features

| Area | What it does |
|---|---|
| Boot | Raw 16-bit real-mode and 64-bit long-mode guests; Linux bzImage + initramfs via the Linux boot protocol (GDT, identity page tables, `boot_params`, e820) |
| Console | 16550 UART on COM1 wired to the host terminal (raw mode, restored on exit and on signals) |
| virtio-blk | virtio-mmio transport, read/write/flush, `--disk-ro`, ioeventfd/irqfd, every guest address and length checked |
| virtio-net | Backed by a host TAP device, RX flow control, guest-supplied header zeroed on TX |
| SMP | `--vcpus 1..4`: MP table, per-vCPU CPUID topology, LAPIC LINT0/LINT1 wiring, clean stop/kick/join on reboot |
| Hardening | Per-thread seccomp allowlists (main, vCPU, stdin) derived from strace, `ioctl` filtered by request, no `PROT_EXEC` mappings, `PR_SET_NO_NEW_PRIVS` |
| Verification | 7 Kani model-checking harnesses over the queue/descriptor/request parsers, plus a libFuzzer target for the virtio-blk path |

A guest is treated as untrusted: nothing it does (any port write, MMIO access or byte value)
may panic the VMM. Every `unsafe` block carries a `// SAFETY:` comment.

## Quick start (prebuilt release)

Requirements: x86_64 Linux with `/dev/kvm` and your user in the `kvm` group
(`sudo usermod -aG kvm $USER`, then log in again). WSL2 with nested virtualization works.

```bash
tar xzf microvm-rs-*-x86_64-linux.tar.gz
cd microvm-rs-*-x86_64-linux
./run.sh                 # boots Linux with a disk and 2 vCPUs; type `reboot -f` to exit
VCPUS=4 ./run.sh --tap vmtap0   # 4 vCPUs + network (see below)
```

Or call the binary directly:

```bash
./vmm boot --kernel bzImage --initrd initramfs.cpio            # serial console only
./vmm boot --kernel bzImage --initrd initramfs.cpio \
           --disk disk.ext4 --vcpus 4                          # + disk, 4 CPUs
./vmm raw --image hello64.bin --mode long                      # tiny 64-bit guest
```

Networking needs a TAP device owned by your user, created once per host boot:

```bash
sudo ip tuntap add dev vmtap0 mode tap user "$USER"
sudo ip addr add 172.16.0.1/24 dev vmtap0 && sudo ip link set vmtap0 up
./vmm boot --kernel bzImage --initrd initramfs.cpio --tap vmtap0
# in the guest:
ip addr add 172.16.0.2/24 dev eth0 && ip link set eth0 up && ping -c 3 172.16.0.1
```

## Building from source

```bash
sudo apt install build-essential nasm flex bison bc libelf-dev libssl-dev xz-utils curl \
                 busybox-static cpio e2fsprogs
cargo build --release                 # target/release/vmm
scripts/build-guests.sh               # guests/build/hello{16,64}.bin
scripts/build-kernel.sh               # artifacts/bzImage (Linux 6.12, configs/phase2.config); slow
scripts/build-initramfs.sh            # artifacts/initramfs.cpio (BusyBox)
scripts/build-disk.sh                 # artifacts/disk.ext4 (64 MiB ext4, no root needed)
```

## Testing

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                            # unit + raw-guest tests
cargo test -- --ignored               # Linux-boot integration tests (need artifacts/)
scripts/verify-phase-1.sh             # Phase 1 acceptance criteria, one PASS/FAIL line each
sudo scripts/setup-tap.sh             # once per host boot, for the network checks
scripts/verify-phase-2.sh             # Phase 2 acceptance criteria (incl. Kani + fuzzing)
```

## Project layout

```
src/arch/x86_64/   boot params, GDT, paging, MP table, register setup
src/devices/       serial, i8042, PIO/MMIO buses, TAP, virtio (mmio, queue, parse, blk, net, Kani proofs)
src/vm.rs vcpu.rs event_loop.rs seccomp.rs loader.rs main.rs
tests/             m1..m7 integration tests
fuzz/              cargo-fuzz target for virtio-blk request parsing
specs/phase-N/     spec, plan and decision log per phase
notes/             learning notes and interview questions per phase
docs/              user manual (PDF)
study-notes/       concept notes, level by level
```

## Status

| Phase | Outcome | Status |
|---|---|---|
| 1 | Boot Linux to a BusyBox shell over an emulated serial console | Complete |
| 2 | virtio-blk/net, multiple vCPUs, seccomp, Kani + fuzzing | Complete |
| 3 | Confidential computing (SEV-SNP / TDX) | Not started |

This is a learning project: one serial console, at most one disk, one NIC and four vCPUs. It
does not include a jailer (chroot/namespaces/cgroups) or an API server. See
[docs/user-manual.pdf](docs/user-manual.pdf) for a guided tour of every command.

## License

[MIT](LICENSE). Release bundles also contain a Linux kernel and a BusyBox initramfs, which
are GPL-2.0 software; their exact versions and build configuration are in `scripts/` and
`configs/phase2.config`.
