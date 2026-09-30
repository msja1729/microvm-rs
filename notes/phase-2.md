# Phase 2 — Learning notes

## What each milestone does

### M4 — virtio-blk
Phase 1's guest had only a serial port and a RAM disk that vanished at every reboot. M4 gives it a real disk. The guest's Linux driver talks to a *virtio* device: a device designed for virtual machines, where the guest and the VMM share queues of buffer descriptions in guest RAM instead of emulating a physical disk controller register by register. The device appears at a fixed guest-physical window (`0xd000_0000`), which the VMM announces on the kernel command line (`virtio_mmio.device=4K@0xd0000000:5`). The guest's reads and writes of that window exit to the VMM, which answers from the *virtio-mmio transport*: identification, feature negotiation (version 1, flush, read-only), and where the guest put its queue. The guest describes each disk request as a chain of three buffers (a header saying read/write/flush and which sector, the data, a status byte) and rings a doorbell. The VMM checks every guest-supplied address and length, copies data between guest RAM and a host image file through a small fixed buffer, writes the status, marks the request done, and raises IRQ 5. With this came a new thread layout: each vCPU runs on its own thread and the main thread became an event loop, because the disk work must not happen on the thread that runs guest code.

### M5 — virtio-net
M5 adds a network card built the same way. On the host side it is backed by a TAP interface, a virtual Ethernet port that a normal user may attach to once root has created it for them (`scripts/setup-tap.sh`). The guest has two queues: one of empty buffers the VMM fills with frames arriving from the host (RX), and one of frames the guest wants to send (TX). Every frame travels with a 12-byte virtio-net header; the VMM configures the TAP to use exactly that header size, turns all offloads off, and zeroes the header of every outgoing frame, so nothing the guest writes there can steer the host's network stack. The main thread watches the TAP for incoming frames, but only while the guest has buffers ready, so it never spins on a frame it can't deliver. The result: guest and host can ping each other and transfer files over `172.16.0.0/24`.

### M6 — Multiple vCPUs
M6 lets the guest use up to four CPUs. KVM runs each vCPU on its own host thread; the VMM's work is to describe the machine so Linux will start and use them. It writes an Intel MultiProcessor table into the last KiB of base memory: one entry per CPU, the IOAPIC, and how the 16 legacy interrupt lines are wired. It gives each vCPU its own CPUID answers (its own APIC ID, and a consistent "one socket, N cores" topology), and it points each local APIC's LINT0/LINT1 pins at the PIC and NMI. Only CPU 0 gets starting registers; the others wait inside KVM until Linux wakes them with INIT/startup IPIs. The MP table is written in every boot, so even a one-CPU guest now routes interrupts through the IOAPIC. When the guest reboots, the VMM stops every vCPU thread (kicking any that are idle inside `KVM_RUN` with a signal) and joins them before exiting.

### M7 — Hardening
A guest is untrusted, and a bug in device emulation could let it run code inside the VMM. M7 limits the damage with seccomp: each long-lived thread installs a filter listing the only system calls it may make, and any other call kills the whole VMM with `SIGSYS`. The vCPU threads may only run the guest (`ioctl(KVM_RUN)`), write output, and synchronize; only the main thread may read and write the disk file and the TAP; no thread can open files, create sockets or start programs. The lists were derived from traces of the real VMM rather than guessed, which caught calls a code review would miss (glibc's `tcsetattr` also reads the terminal settings). `--no-seccomp` turns the filters off for debugging, with a warning.

### M8 — Verification
The code that reads guest-controlled data (queue indices, descriptor chains, blk request headers, net TX chains) is written as pure functions over a small memory trait, with no KVM in sight. That makes it checkable by tools. Kani, a model checker, proves properties for *every* possible input within bounds (queue size 4, fully symbolic descriptors, memory size and disk capacity): the parsers never panic or overflow, every accepted range lies inside guest RAM and the disk, loops and `INDIRECT` chains are rejected within a bounded number of steps, and the ring code never reads outside the rings. A fuzzer (libFuzzer via `cargo-fuzz`) complements it by throwing millions of random guest-memory images with a larger queue (16) at the whole blk path.

## How a virtio request travels

```
guest driver                          VMM (main thread: event loop)                 host
------------                          -----------------------------                 ----
build descriptor chain in RAM
put head index in avail ring
avail.idx += 1
writel(q, base+0x50)  --KVM_IOEVENTFD-->  queue eventfd readable
  (no VM exit to user space)               epoll_wait returns
                                           lock transport
                                           Queue::pop: read avail.idx, slot (checked)
                                           walk_chain: copy descriptors (bounded)
                                           parse_blk_request / parse_net_tx (checked)
                                           blk: pread/pwrite via 64 KiB buffer  ----->  disk image
                                                fdatasync for FLUSH             ----->  (durable)
                                           net TX: gather, zero header, write  ----->  TAP -> host stack
                                           write status byte, used elem,
                                           Release fence, used.idx
                                           InterruptStatus |= 1; write irqfd
vm_interrupt  <--KVM_IRQFD: IRQ 5/6 via IOAPIC--
read InterruptStatus (vCPU exit, lock-free)
write InterruptACK
scan used ring, complete request
```
Net RX runs the other way: the host kernel queues a frame on the TAP; the event loop wakes on the TAP fd only when the guest has posted RX buffers, reads one frame, scatters it into the next RX chain, publishes it in the used ring and raises IRQ 6. Register accesses other than `QueueNotify` (feature negotiation, queue setup, `Status`) are MMIO exits handled on the vCPU thread that made them, under the same transport lock.

## Seccomp allowlists

Every long-lived thread installs its own filter (`src/seccomp.rs`) before it handles guest-controlled data: each vCPU thread before its first `KVM_RUN`, the stdin thread before its first `read`, and the main thread right before the event loop, after every file, eventfd, KVM object and thread exists. `PR_SET_NO_NEW_PRIVS` is set once, before the first thread is spawned. Any syscall not listed kills the whole process (`SECCOMP_RET_KILL_PROCESS`, `SIGSYS`). `--no-seccomp` turns all of this off, for debugging only.

**How the lists were derived.** Before any filter existed (the same syscalls `--no-seccomp` makes), `vmm boot --vcpus 4 --disk … --tap vmtap0` ran under `strace -f -ff -qq -o t` with a workload covering every device path: mount, a 4 MiB write, `sync` (virtio-blk FLUSH → `fdatasync`), `umount`, guest network setup, `ping` (TX and RX), an HTTP download, and `reboot -f` (the stop/kick/join path). A second trace sent `SIGTERM` to a running VMM (the terminal-restoring signal handler). Each `-ff` trace file was assigned to a thread type by its `prctl(PR_SET_NAME, …)` line (the file with `execve` is main; only syscalls after main's last `clone3` count). The union per thread type is the list below. Rare paths were checked on purpose: the kick (`tgkill`, `rt_sigreturn`), flush, and the signal exit. The traces showed two things the plan had missed: glibc's `tcsetattr` issues `TCGETS` as well as `TCSETS`, and a finished vCPU thread closes its vCPU fd (`fcntl F_GETFD` + `close` + `munmap` of `kvm_run`). Allocator syscalls are allowed as a family on every thread, because malloc chooses among them depending on heap state rather than on the code path. `mmap`/`mprotect` are allowed only without `PROT_EXEC`. `ioctl` is argument-filtered to the listed requests.

### main thread
| Syscall | Why |
|---|---|
| `epoll_wait` | the event loop waits for eventfds and the TAP |
| `epoll_ctl` | start/stop watching the TAP (RX flow control) |
| `read` | drain eventfds; read frames from the TAP |
| `write` | irqfd signals, TAP frames, stderr, the signal handler's message |
| `pread64` | virtio-blk IN (`FileExt::read_exact_at`) |
| `pwrite64` | virtio-blk OUT (`FileExt::write_all_at`) |
| `fdatasync` | virtio-blk FLUSH (`File::sync_data`) |
| `getpid` | `pthread_kill` (vCPU kick) looks up the process id |
| `tgkill` | `pthread_kill`: send `SIGRTMIN` to a vCPU thread |
| `clock_nanosleep` | the 10 ms pause between vCPU kicks |
| `fcntl` | `F_GETFD`: std's debug-build check before closing a file |
| `close` | closing files, eventfds, the TAP and KVM fds at exit |
| `ioctl` | only `TCGETS`, `TCSETS`: restoring the terminal (exit path and signal handler) |
| `exit_group` | process exit; the signal handler's `_exit(1)` |
| `rt_sigreturn` | return from a signal handler |
| `futex` | mutex waits and wake-ups; thread join |
| `rt_sigprocmask` | glibc blocks signals around `pthread_kill` |
| `sigaltstack` | std removes the signal stack at exit |
| `mmap`, `mprotect` | allocator (never with `PROT_EXEC`) |
| `munmap`, `madvise`, `brk`, `mremap` | allocator; unmapping guest RAM at exit |

### vCPU threads
| Syscall | Why |
|---|---|
| `ioctl` | only `KVM_RUN`, plus `TCGETS`, `TCSETS` for the signal handler |
| `write` | serial output to stdout, the serial irqfd, the exit eventfd, logs, the signal handler's message |
| `futex` | the port-I/O bus, serial and virtio transport mutexes |
| `rt_sigreturn` | return from the no-op `SIGRTMIN` kick handler |
| `exit_group` | the signal handler's `_exit(1)` |
| `fcntl` | `F_GETFD`: std's debug-build check before closing the vCPU fd |
| `close` | dropping the vCPU fd when the thread ends |
| `exit` | the thread ends |
| `rt_sigprocmask` | glibc blocks signals during thread exit |
| `sigaltstack` | std removes the thread's signal stack when it ends |
| `mmap`, `mprotect` | allocator (never with `PROT_EXEC`) |
| `munmap`, `madvise`, `brk`, `mremap` | allocator; the `kvm_run` page and the thread stack at exit |

### stdin thread
| Syscall | Why |
|---|---|
| `read` | read host stdin |
| `write` | the serial irqfd when input arrives, logs, the signal handler's message |
| `futex` | the serial mutex |
| `clock_nanosleep` | wait 1 ms while the guest's RX FIFO is full |
| `ioctl` | only `TCGETS`, `TCSETS`, for the signal handler |
| `exit_group` | the signal handler's `_exit(1)` |
| `rt_sigreturn` | return from a signal handler |
| `exit` | the thread ends on stdin EOF or error |
| `rt_sigprocmask` | glibc blocks signals during thread exit |
| `sigaltstack` | std removes the thread's signal stack when it ends |
| `mmap`, `mprotect` | allocator (never with `PROT_EXEC`) |
| `munmap`, `madvise`, `brk`, `mremap` | allocator; the thread stack at exit |

Not a VMM thread: KVM itself adds a `kvm-nx-lpage-recovery` worker to the VM owner's process. It copies the creating thread's seccomp and no-new-privs state, and the M7 test checks that it, too, reports `Seccomp: 2`.

## Kani properties and their limits

`cargo kani` checks seven harnesses in `src/devices/virtio/proofs.rs` (about 1 minute in total here; the bound is 20). Queue size 4, so chains of up to 4 descriptors; loop unwinding 5 (and Kani's unwinding assertions prove no loop needs more). Guest memory is `KaniMem`: its size is symbolic and every in-bounds read returns arbitrary bytes, so every descriptor address, length, flag and next index, ring index, request header and disk capacity is symbolic. Arithmetic overflow, out-of-bounds indexing and panics are checked automatically in every harness.

| Harness | Proves |
|---|---|
| `pop_stays_in_ring` | (e) `pop` reads only `avail.idx` and ring slots; a returned head is `< size` and advances the position by one; an `avail.idx` more than `size` ahead is rejected; `add_used` writes only inside the used ring |
| `walk_chain_bounded` | (d)+(e) the walk ends within `size` iterations for *any* descriptor contents, reads only table entries with index `< size`, returns 1..=size descriptors, rejects `INDIRECT`; with a stable table, a chain revisiting an index is rejected |
| `blk_parse_no_panic` | (a) the blk parser never panics or overflows |
| `blk_parse_ranges_in_memory` | (b) every data range and the status byte of an accepted request lie inside guest memory (checked in `u128`) |
| `blk_parse_sector_bounds` | (c) accepted IN/OUT requests satisfy `sector × 512 + len ≤ capacity × 512` in exact arithmetic, so no `u64` overflow was possible, and every out-of-range request is rejected |
| `net_tx_bounds` | (f) TX parsing never panics; an accepted chain's length is in `[12, 65 562]`, equals the sum of its buffers, all inside memory |
| `net_tx_header_zeroed` | (f) `zero_vnet_header` never panics on any buffer length up to 64 and leaves the header zero and the rest unchanged |

The proofs were checked against mutations: with the `INDIRECT` rejection removed from `walk_chain`, or the upper TX length check removed from `parse_net_tx`, the corresponding harness fails.

### What the proofs do not cover
- Only queue size 4. Nothing in the code depends on the size beyond `< size` comparisons and `% size`, but chains of up to 256 descriptors are argued, not proven; the fuzzer exercises size 16.
- `KaniMem` returns arbitrary bytes, not a real memory: it proves "for any values the guest could present", not anything about a particular guest's data, and it does not model the host mmap, `vm-memory`, or real concurrency (only its effect: values changing between reads).
- The code that acts on parsed requests is not proven: the block device's file I/O (`block.rs`), the net RX scatter and TAP I/O (`net.rs`), the virtio-mmio transport registers (`mmio.rs`), the event loop, the MMIO/PIO buses, the MP table and CPUID code, and seccomp. They are covered by unit and integration tests only.
- `has_available` and the RX chain parser `parse_net_rx` have no harness (they are simple and unit-tested).
- Liveness and ordering (memory fences, interrupt delivery, lost notifications) are outside what these harnesses state.
- The fuzzer covers the blk path only (pop → walk → parse → add_used), with inputs up to libFuzzer's default length, where the rings and descriptors live.

## Bugs found by Kani and fuzzing

No bugs in the VMM code were found by Kani or by the fuzzer (1.36 M executions in the first 30 s smoke run, plus the 300 s verify run), so there are no regression tests for them: none were found. Two things Kani did find, both in the proof harnesses, not in the code:
- With a chain of symbolic *length* (a `Vec` sliced at a symbolic index), `net_tx_bounds` alone ran past 15 minutes in the SAT solver. Harnesses now branch over each concrete length 0..=4 instead, with the same coverage; all seven finish in about a minute.
- `walk_chain_bounded` first asserted that any chain visiting a descriptor index twice is rejected. Kani produced a counterexample: head 0 has `NEXT` → 0, and when the walker reads descriptor 0 again the (concurrently rewritten) bytes no longer have `NEXT`, so a 2-descriptor chain is accepted. That is not a safety bug: every descriptor read is in the table and checked, the walk is still bounded by `size`, and the spec defines the loop to reject as "a chain with more descriptors than the queue size", which is what the walker enforces. In a table the guest does not rewrite, a revisit is an infinite cycle, and the harness proves it is rejected. The harness now states exactly that, and AC-8.2(d) was reworded to match (D42).

## Fuzz seed corpus

The five seeds in `fuzz/corpus/blk_request/` (IN, OUT, FLUSH, GET_ID, a 2-descriptor loop) were generated once, from the repository root, with:

```python
import struct
NEXT, WRITE = 1, 2
def seed(name, descs, rtype=None, sector=0, capacity=128):
    mem = bytearray(0x610)
    for i, (addr, ln, flags, nxt) in enumerate(descs):
        mem[16*i:16*i+16] = struct.pack("<QIHH", addr, ln, flags, nxt)
    mem[0x100:0x106] = struct.pack("<HHH", 0, 1, 0)       # avail: flags, idx=1, ring[0]=head 0
    if rtype is not None:
        mem[0x400:0x410] = struct.pack("<IIQ", rtype, 0, sector)  # blk header
    open(f"fuzz/corpus/blk_request/{name}", "wb").write(struct.pack("<Q", capacity) + mem)
hdr, data, status = (0x400, 16, NEXT, 1), (0x1000, 512, NEXT | WRITE, 2), (0x600, 1, WRITE, 0)
seed("in", [hdr, data, status], rtype=0, sector=3)
seed("out", [hdr, (0x1000, 512, NEXT, 2), status], rtype=1, sector=5)
seed("flush", [hdr, (0x600, 1, WRITE, 0)], rtype=4)
seed("get_id", [hdr, (0x1000, 20, NEXT | WRITE, 2), status], rtype=8)
seed("loop", [(0x400, 16, NEXT, 1), (0x600, 1, NEXT | WRITE, 0)], rtype=0)
```
Input format: 8 bytes of capacity (sectors, LE), then the guest-memory image (zero-padded to 64 KiB): descriptor table at 0x0, available ring at 0x100, used ring at 0x200, queue size 16.

## Interview questions

### M4 — virtio-blk

**Q1. A guest's `QueueNotify` write never reaches the VMM's MMIO handler. Trace one disk read from the guest driver to the host file and back to the guest's interrupt handler, naming the thread that does each step.**
The guest's virtio_blk driver writes three descriptors (16-byte header with `type = IN` and the sector, the data buffer, the 1-byte status), puts the head index in the available ring, bumps `avail.idx`, and writes the queue index (0) to `QueueNotify` at `0xd000_0050`. KVM matches that 4-byte write against the ioeventfd the VMM registered with `KVM_IOEVENTFD` (datamatch 0), signals the eventfd and resumes the guest at once: no exit to user space, and the vCPU thread never sees it. On the **main thread**, epoll wakes the event loop. The `QueueSubscriber` drains the eventfd, locks the transport and calls `Block::process_queue`. `Queue::pop` reads `avail.idx` and the ring slot, `walk_chain` copies the descriptors out, and `parse_blk_request` checks directions, RAM ranges and the disk range. `copy_to_guest` then `pread`s the file into the 64 KiB bounce buffer and copies it into guest RAM chunk by chunk. The device writes the status byte, then the used element, a release fence, and `used.idx`. It sets bit 0 of `InterruptStatus` and writes the irqfd. KVM injects IRQ 5. The guest's `vm_interrupt` runs on the **vCPU thread**: its `InterruptStatus` read and `InterruptACK` write do exit to the VMM, but they are served from an atomic without taking the transport lock. The driver then reads the used ring and completes the block request.

**Q2. Every descriptor field is guest-controlled. Name the ways a malicious chain could hurt the VMM, the check that stops each one, and why every descriptor is read exactly once.**
A `next` loop would spin forever: `walk_chain` gives up after `size` descriptors (`TooLong`), which is the only way a chain can revisit an index. An index ≥ queue size would read outside the descriptor table: it is rejected (`BadIndex`), and a head index ≥ size in the available ring marks the whole queue broken. An `avail.idx` more than `size` ahead is a corrupt ring (`Broken`). `INDIRECT` is never negotiated, so it is rejected. An address near `u64::MAX` or past RAM: `addr.checked_add(len) ≤ mem.size()` for the header, every data buffer and the status byte, so the check can't wrap. A huge sector: `sector.checked_mul(512)` and `checked_add(total)` must stay ≤ the disk's byte size. A 4 GiB `len` never sizes an allocation, because data moves through a fixed 64 KiB bounce buffer. A read-only disk is enforced by the device, not by what the guest negotiated. Reading each structure once into a local copy defeats time-of-check/time-of-use: the guest's other vCPUs can rewrite descriptors while the VMM works, and a second read could see a different, unchecked value.

**Q3. With only one vCPU, why did M4 already need a stop / kick / join protocol, and why is the kick signal resent every 10 ms instead of sent once?**
`KVM_RUN` blocks, so the event loop can't share a thread with the vCPU. The vCPU moved to its own thread and the main thread became the event loop. Usually the vCPU ends the VM itself: it sees the i8042 reset, records the outcome and returns. But when something else ends the VM (the event loop failing, a poisoned lock, or with more vCPUs another vCPU), the vCPU may be blocked inside `KVM_RUN`, for example with the guest idle in `HLT`, and `join` would hang forever. So main sets a `stop` flag and sends `SIGRTMIN`. Its handler is a no-op installed without `SA_RESTART`, so `KVM_RUN` returns `EINTR` and the loop sees `stop`. A single signal can be lost: if it arrives after the thread checked `stop` but before it entered `KVM_RUN`, the handler runs, nothing is interrupted, and the thread then blocks in `KVM_RUN`. Resending every 10 ms until the thread finishes closes that race without `immediate_exit` or thread-local state in a signal handler. A 5 s deadline turns a truly stuck thread into exit 1 instead of a hang.

### M5 — virtio-net

**Q1. The TAP is opened with `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`, and the VMM sets `TUNSETVNETHDRSZ` to 12. What would break if it didn't, and why is the guest's TX header zeroed before it reaches the TAP?**
`IFF_TAP` gives Ethernet frames (not IP packets), which is what a NIC carries. `IFF_NO_PI` removes the 4-byte packet-information prefix; with it, every frame would be misaligned by 4 bytes. `IFF_VNET_HDR` makes the TAP exchange a virtio-net header in front of each frame, so guest buffers can be handed over as-is. The kernel's default header is the 10-byte legacy one, but with `VIRTIO_F_VERSION_1` the guest always uses 12 bytes (with `num_buffers`). Without `TUNSETVNETHDRSZ 12`, every TX frame would reach the host with 2 header bytes counted as the start of the Ethernet destination MAC, and every RX frame would land 2 bytes early in the guest. Nothing would work, though nothing would crash. The header fields (GSO type, checksum start and offset) are instructions to the host kernel. The guest negotiated no offloads, so any non-zero value is either garbage or an attack on the host network stack. Zeroing the whole header means guest-controlled bytes never steer host kernel code; only the frame itself goes through.

**Q2. The event loop is level-triggered epoll. Why does the VMM stop watching the TAP when the RX queue has no buffers, and what turns it back on?**
With level triggering, epoll reports the TAP readable for as long as a frame is queued. If the guest has no free RX buffers, the VMM can't take the frame, so the next `epoll_wait` returns immediately and the main thread spins at 100% CPU. The VMM checks `has_available` before reading a frame (so frames wait in the host kernel's TAP queue, not in the VMM). When an RX pass ends with no guest buffers left, the subscriber removes the TAP from the interest list. Linux's virtio_net refills RX buffers as it consumes packets and kicks the RX queue. That notification reaches the event loop through the RX ioeventfd, the RX pass runs again (delivering waiting frames), and the TAP is watched again as long as that pass didn't run dry. TX never needs this: the VMM writes to the TAP and, if the TAP is full (`EAGAIN`), drops the frame, which Ethernet allows.

**Q3. A malicious guest posts a TX chain of 256 descriptors, each claiming 4 GiB. What stops it from exhausting host memory or overflowing a length, and how does that differ from the RX side?**
`parse_net_tx` checks every descriptor's direction and that `addr + len` lies inside guest RAM, which rejects a 4 GiB buffer in a 512 MiB guest immediately. It also sums lengths with `checked_add` and rejects the chain as soon as the running total passes 65,562 bytes (12-byte header + 65,550-byte frame), before the sum could ever overflow. The frame is gathered into a single reused 65,562-byte buffer allocated at startup, so no allocation depends on a guest value. A rejected chain is still completed (used length 0) so the ring keeps moving. On RX the guest supplies writable buffers; their total can be anything, but the VMM only writes the frame it read, at most one buffer's worth. If the frame doesn't fit, it is dropped and the chain completed with length 0, so an undersized chain can't cause a partial or out-of-bounds write.

### M6 — Multiple vCPUs

**Q1. The VMM creates four vCPUs but only sets registers on vCPU 0. How do the other three start running Linux, and what are their threads doing until then?**
x86 SMP boots like real hardware: the bootstrap processor (vCPU 0) starts at the kernel entry point with the VMM's long-mode registers. The application processors are created in KVM's initial `KVM_MP_STATE_UNINITIALIZED` state, which is "waiting for INIT/SIPI". Each AP's thread calls `KVM_RUN`, and KVM blocks it inside the kernel because the CPU isn't runnable yet. Linux on the BSP reads the MP table to learn the APs' APIC IDs, copies a real-mode trampoline below 1 MiB, and sends each AP an INIT IPI and then a Startup IPI whose vector gives the trampoline's page. The in-kernel LAPIC emulation handles these IPIs, resets the AP to real mode at `vector << 12`, and makes it runnable, and the AP's `KVM_RUN` starts executing guest code. The VMM never sees any of this. If the VMM set boot registers on an AP, it would start at the kernel entry point as a second BSP and corrupt the boot.

**Q2. Why does every vCPU need its own CPUID table, and which leaves describe the topology?**
The guest finds out which CPU it is running on, and how CPUs group into cores and packages, from CPUID. KVM's supported table is the host's view: host APIC ID, host core counts, host cache sharing. If every vCPU reported the same initial APIC ID (leaf 1 EBX[31:24]) or x2APIC ID (leaf 0xB/0x1F EDX), Linux would think they are the same CPU or mis-number them against the MP table and LAPIC IDs, which KVM sets to the vCPU index. Leaf 1 also carries the logical-processor count and the HTT flag. Leaf 4 describes each cache and how many cores share it. Leaves 0xB and 0x1F give the x2APIC topology levels (SMT, then core) with their shift widths. The VMM rewrites them per vCPU for "1 socket, N cores, 1 thread per core, APIC ID = index". Otherwise the scheduler builds its domains from a topology that doesn't exist (for example 24 threads on a 4-vCPU guest).

**Q3. `reboot -f` runs on whichever CPU the shell happens to be on. How does the VMM make sure all four vCPU threads stop, including ones halted in the guest, and why can't it just call `exit()`?**
The vCPU that sees the i8042 reset write returns `GuestStop::Reset` and records the outcome in `VmExit` (first writer wins), which writes the exit eventfd. The main thread's event loop wakes up, sets the shared `stop` flag and sends `SIGRTMIN` every 10 ms to each vCPU thread that hasn't finished. Idle CPUs are blocked inside `KVM_RUN` (the guest executed `HLT` and the in-kernel LAPIC is waiting for an interrupt); the signal, with a no-op handler and no `SA_RESTART`, makes `KVM_RUN` return `EINTR`, and the loop sees `stop` and returns. Then main joins every thread, with a 5 s deadline. `exit()` would skip that: device state would be torn down while other threads may still be touching guest memory or device locks, destructors (the terminal guard) could race with a vCPU writing to stdout, and a vCPU mid-way through writing the disk could leave it inconsistent. Joining gives one ordered shutdown. AC-6.5 shows the whole thing takes well under 10 s with 4 vCPUs.

### M7 — Hardening

**Q1. Why does each thread type get its own seccomp filter, why is each filter installed by the thread itself, and why is `PR_SET_NO_NEW_PRIVS` set before any thread is spawned?**
Seccomp filters are per-thread. A thread inherits the filters its creator had when it was created, and installing a filter affects only the calling thread (unless `TSYNC` is used). So each thread installs its own filter as its first step, before it touches guest-controlled data: the vCPU before its first `KVM_RUN`, the stdin thread before its first `read`, and main just before the event loop, once every fd it will ever need exists. Separate filters give least privilege: only vCPU threads can `ioctl(KVM_RUN)`, only main can `pread`/`pwrite` the disk or `tgkill`, and no thread can `open`, `socket` or `execve`. A guest exploiting a bug in the MMIO path on a vCPU thread can't touch the disk file directly. Installing a filter without `CAP_SYS_ADMIN` requires no-new-privs, which guarantees the process can never gain privileges later (for example by exec'ing a setuid binary to escape the filter). It is set once in main before the first spawn, so every thread inherits it, and the M7 test checks `NoNewPrivs: 1` on every thread.

**Q2. The allowlists were derived from `strace` rather than written from knowledge of the code. What did the traces reveal that reading the code would have missed, and what is the risk of an allowlist that's too tight?**
The code calls `tcsetattr`, `File::drop`, `pthread_kill` and `thread::sleep`. The syscalls underneath depend on glibc and std internals. glibc's `tcsetattr` does `TCGETS`, `TCSETS`, `TCGETS`, not just `TCSETS`. `pthread_kill` is `rt_sigprocmask` + `getpid` + `tgkill`. std's debug build does `fcntl(F_GETFD)` before every `close`. A vCPU thread that finishes closes its vCPU fd and unmaps `kvm_run`. Thread exit uses `sigaltstack`, `munmap`, `madvise` and `exit`. The main thread's clean exit unmaps 512 MiB of guest RAM. The mismatch action is `KILL_PROCESS`, so a missing entry isn't a failed syscall but a dead VMM with no terminal restore, possibly only on a rare path (a disk flush, a signal, heap growth under pressure). That is why the traces exercised the rare paths on purpose, and why the allocator syscalls are allowed as a family (with `PROT_EXEC` blocked) rather than exactly as traced.

**Q3. What does seccomp not protect against here, and how does a jailer-style design (out of scope for Phase 2) complement it?**
Seccomp limits which syscalls a thread can make, and some of their arguments (`ioctl` requests, `PROT_EXEC`), but not what the allowed ones do. The main thread may still `pwrite` anywhere in the disk file and `write` to the TAP. A vCPU thread may still `write` to any fd it holds, including stdout and eventfds. Memory corruption inside the VMM can still misuse any open fd or read secrets from process memory. The filter also doesn't hide the host filesystem from fds opened before it was installed, and it doesn't limit CPU or memory use. A jailer adds the missing layers: it starts the VMM in a chroot with only the files it needs, in its own mount/PID/network namespaces, as an unprivileged dedicated uid, with cgroup limits. Seccomp then shrinks the kernel attack surface inside that box. Firecracker uses both. This VMM has only the seccomp half.

### M8 — Verification

**Q1. What exactly did Kani prove about the descriptor-chain walker, and why is "for every possible input" different from what the fuzzer gives you?**
For a queue of size 4 and *every* possible combination of descriptor-table address, head index, memory size and descriptor bytes (including bytes that change between reads), `walk_chain` terminates within 4 iterations (Kani's unwinding assertions fail if any loop could run longer), reads only 16-byte-aligned table entries with index < 4, returns 1 to 4 descriptors, and rejects any `INDIRECT` descriptor; with a stable table it rejects any chain that revisits an index. Kani turns the code and the property into a SAT problem and shows no satisfying counterexample exists, so it is a proof over the whole (bounded) input space. The fuzzer instead runs the real code on millions of concrete inputs, guided by coverage; it can use a bigger queue (16), the real `VecMem`, and the whole pop → walk → parse → add_used pipeline, but it can only show bugs, never their absence. They complement each other: Kani for exhaustive guarantees on small bounds, fuzzing for breadth on realistic sizes and for code paths the proofs don't model.

**Q2. Why is guest memory modelled as "every read returns arbitrary bytes" rather than as a byte array, and what did that model reveal?**
A symbolic byte array the size of guest RAM would make the solver reason about up to 2^64 addresses; `KaniMem` avoids that entirely by keeping only a symbolic size and returning fresh arbitrary bytes on each in-bounds read. That is also a faithful model of the real threat: other guest vCPUs can rewrite descriptors while the VMM reads them, so two reads of the same address really can disagree. It revealed that "a chain that revisits an index is rejected" is only true when the table is stable: with a concurrent rewrite, a revisited descriptor can end the chain early. That is safe (the code reads each descriptor once into a local copy, checks it, and the walk stays bounded), and it is why the code defines a loop as "more than `size` descriptors" rather than "an index seen twice". The harness adds an explicit stable mode to prove the loop property where it is meaningful.

**Q3. The first version of one harness took over 15 minutes; after a change it took 10 seconds with the same coverage. What changed, and what does that teach about writing proofs?**
The harness built its input as `all[..n].to_vec()` with a symbolic `n`. That made every later structure (the parser's `Vec` of ranges, the harness's sums over it) heap data of symbolic length, which CBMC encodes with symbolic pointer arithmetic and many same-allocation checks, and the SAT problem blew up. Branching on `n` with a `match` and passing slices of *concrete* length 0, 1, 2, 3, 4 covers exactly the same inputs, but each branch has fixed sizes that constant-propagate, so the problem is small. A related lesson came from the unwinding assertions on a harness loop over a `Vec`: an `assert!` about its length doesn't bound the loop for the unwinder, while looping over a constant bound does. Proofs are programs for a solver: keep sizes concrete where the property allows it, make the symbolic parts the values the attacker controls, and check the harness can fail (mutation testing) before trusting a "SUCCESSFUL".
