# Phase 2 — Learning notes

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
