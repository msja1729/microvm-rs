# Phase 2 Decisions

Choices made inside the spec. Format: question → options → choice → why. D1–D18 were made while writing `PLAN.md` and take effect once the plan is approved.

## Planning

### D1. Thread layout
- **Options:** keep vCPU 0 on the main thread and add an event-loop thread; run every vCPU on its own thread and make main the event loop.
- **Chose:** main thread = setup, then the event loop; one thread per vCPU; the `stdin` thread as in Phase 1.
- **Why:** every vCPU is treated the same (no special-cased vCPU 0 thread), and main is the natural place to collect the exit outcome and join the vCPUs.

### D2. Stopping the vCPUs
- **Options:** `process::exit` without joining; `immediate_exit` set from a signal handler through thread-local state; a stop flag plus repeated `SIGRTMIN` kicks, then join.
- **Chose:** a stop flag checked before each `KVM_RUN`, with `SIGRTMIN` resent every 10 ms to unfinished vCPU threads (no-op handler, no `SA_RESTART`), and a join with a 5 s deadline.
- **Why:** pitfall 7 requires stop and join. Resending closes the race of a signal arriving just before `KVM_RUN` without thread-local `kvm_run` access in a signal handler. It also interrupts APs blocked waiting for SIPI.

### D3. virtio-mmio register layout
- **Options:** legacy version 1; version 2; both.
- **Chose:** version 2 only.
- **Why:** `VIRTIO_F_VERSION_1` is offered and required (Q3), and Linux's virtio_mmio driver supports version 2. Legacy adds page-size/PFN registers for no benefit.

### D4. Queue size limit and interrupt suppression
- **Options:** `QueueNumMax` 128/256/1024; honour or ignore `VIRTQ_AVAIL_F_NO_INTERRUPT`.
- **Chose:** `QueueNumMax` 256; always interrupt after using buffers.
- **Why:** 256 is Linux's usual ring size and Firecracker's. The no-interrupt flag is only a hint; ignoring it is allowed and removes a lost-wakeup risk.

### D5. Handling rejected chains and corrupt rings
- **Options:** mark the whole queue broken on any bad chain; complete the bad chain and continue.
- **Chose:** a bad chain (loop, `INDIRECT`, bad index, bad request) is completed with used length 0 (blk also writes `VIRTIO_BLK_S_IOERR` when a valid status byte exists). A corrupt ring (`avail.idx` jump > queue size, head index ≥ size) stops the queue and sets `DEVICE_NEEDS_RESET` with a config-change interrupt.
- **Why:** both are within the spec's "request fails with an error status, or the queue is marked broken". A single bad request shouldn't kill the disk, but a corrupt ring has no safe way forward.

### D6. Moving disk data
- **Options:** allocate a buffer per descriptor; direct volatile slices into guest memory; a reused fixed-size bounce buffer.
- **Chose:** one reused 64 KiB bounce buffer, copied chunk by chunk with `read_at`/`write_at`.
- **Why:** a descriptor can be up to 4 GiB, so per-descriptor allocation would let the guest drive host memory use. Chunking needs no `unsafe`.

### D7. RX when the guest has no buffers or a frame doesn't fit
- **Options:** keep reading and drop frames; hold one pending frame; stop watching the TAP.
- **Chose:** stop watching the TAP until the RX queue is notified; drop a frame that doesn't fit its chain and complete the chain with length 0.
- **Why:** epoll is level-triggered, so watching a readable TAP with nowhere to put the frame spins the event loop. With MTU 1500, Linux's RX buffers always fit a frame.

### D8. TX when the TAP is full
- **Options:** block; retry later; drop.
- **Chose:** drop the frame on `EAGAIN` and complete the chain.
- **Why:** Ethernet may drop frames, and the TAP is non-blocking so the event loop never stalls.

### D9. MP table: when and how
- **Options:** add it in M4 (first IRQs beyond serial) or M6 (SMP); build it from `#[repr(C, packed)]` structs with `ByteValued` or as a byte vector.
- **Chose:** M6; a little-endian byte vector with checksums, verified by a unit test that re-parses it.
- **Why:** M4/M5 IRQs 5/6 work through the PIC path Phase 1 already uses, so M4's diff stays scoped. A byte vector needs no `unsafe`.

### D10. CPUID topology
- **Options:** leave the host topology; rewrite Intel and AMD leaves; rewrite Intel leaves only.
- **Chose:** rewrite leaves 1, 4, 0xB and 0x1F for "1 socket, N cores, 1 thread per core, APIC ID = vCPU index"; AMD leaves left as reported and documented as unsupported.
- **Why:** the development host is Intel; AMD support would be untestable here.

### D11. Rerunning M4/M5 checks with 4 vCPUs (AC-6.6, AC-7.3)
- **Options:** duplicate the M4/M5 tests in `tests/m6_smp.rs`; parametrize them.
- **Chose:** the M4/M5 tests read `VMM_TEST_VCPUS` (default 1); the verify script reruns them with `VMM_TEST_VCPUS=4`.
- **Why:** no duplicated test code and no new files.

### D12. Seccomp filter form and the violation trigger
- **Options:** JSON filters compiled at build time; filters written in Rust; any unused syscall for the trigger.
- **Chose:** Rust-code filters in `seccomp.rs` with a comment per syscall; hidden flag `--seccomp-test-violation` makes vCPU 0 call `socket(AF_INET, SOCK_STREAM, 0)` after installing its filter.
- **Why:** Rust code keeps the per-syscall comments next to the rules and needs no build script. No VMM thread ever needs `socket`.

### D13. Fuzz input format
- **Options:** `arbitrary`-derived structures; raw bytes interpreted as guest memory.
- **Chose:** 8 bytes of disk capacity, then a 64 KiB guest-memory image with a fixed queue layout (size 16; desc table 0x0, avail 0x100, used 0x200).
- **Why:** no extra crate, and raw memory lets the fuzzer produce any ring or descriptor state the guest could.

### D14. Seed corpus generation
- **Options:** a Rust helper binary or test; a one-off script kept in the repo; a one-off snippet recorded in the notes.
- **Chose:** a short Python snippet recorded in `notes/phase-2.md`; only the 5 binary seeds are committed.
- **Why:** the output-file list has no generator file, and the seeds rarely change.

### D15. Event loop implementation
- **Options:** `event-manager`; `vmm_sys_util::epoll` directly.
- **Chose:** `event-manager`, falling back to `vmm_sys_util::epoll` if `cargo tree -d` shows a version conflict.
- **Why:** the spec allows `event-manager`; not using an allowed crate needs no spec change.

### D16. Memory trait
- **Options:** `vm-memory`'s `GuestMemory`; a small trait of our own.
- **Chose:** `GuestMem { size, read, write }` with checked bounds; implemented for `GuestMemoryMmap`, `KaniMem` (symbolic size, arbitrary contents) and `VecMem` (fuzzing, unit tests).
- **Why:** AC-8.1/Q2 require a trait Kani can run on. Arbitrary contents on every read also models a guest changing memory concurrently.

### D17. Detecting a missing TAP
- **Options:** rely on `TUNSETIFF` errors; check `/sys/class/net/<name>` first.
- **Chose:** check `/sys/class/net/<name>` (and `IFNAMSIZ`) first, then `TUNSETIFF`.
- **Why:** without `CAP_NET_ADMIN`, `TUNSETIFF` on a missing name fails with `EPERM`, which reads like a permission problem.

### D18. When the vCPU moves off the main thread
- **Options:** M4 (with the event loop); M6 (with SMP).
- **Chose:** M4.
- **Why:** M4 needs the event loop on some thread, and the vCPU can't share one with it because `KVM_RUN` blocks. Doing it once in M4 means M6 only generalizes from 1 to N vCPUs.
