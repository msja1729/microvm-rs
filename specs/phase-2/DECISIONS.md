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

## M4 — virtio-blk

### D19. How the MMIO bus holds devices
- **Options:** `Arc<Mutex<dyn MmioDevice>>` entries (as sketched in the plan); `Arc<dyn MmioDevice>` with `&self` methods, each device doing its own locking.
- **Chose:** `Arc<dyn MmioDevice>` with `&self` methods. The virtio entry (`MmioTransport`) serves `InterruptStatus`/`InterruptACK` straight from the atomic and locks the transport only for other registers.
- **Why:** with a bus-level mutex, every register access, including interrupt status, would wait while the event loop holds the transport lock during disk I/O. That would defeat the plan's "`InterruptStatus` outside the lock" design.

### D20. When disk arguments are validated
- **Options:** while attaching the device (after the kernel is loaded); first thing in `run_boot`.
- **Chose:** first thing in `run_boot`, before `/dev/kvm` is opened. The final command-line length is checked there too.
- **Why:** AC-4.8's exit codes and messages then don't depend on the kernel or KVM, which is what the plan's "no kernel needed" test needs, and a bad argument fails fast.

### D21. `unwrap`/`expect` in unit tests
- **Options:** `clippy.toml` `allow-*-in-tests`; `#![cfg_attr(test, allow(...))]` in `lib.rs`; `Result`-returning tests.
- **Chose:** `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]` in `lib.rs`.
- **Why:** Phase 1 had no in-crate unit tests, so the crate-wide deny never met test code. This keeps the deny for all non-test code without adding a file outside the spec's output list.

### D22. Transport rules the plan left open
- **Options:** accept driver writes at any time; freeze state once the driver commits to it.
- **Chose:** `DriverFeatures` is ignored after `FEATURES_OK`. All queue registers, including `QueueReady`, are ignored while the device is active (after an accepted `DRIVER_OK`, until reset). `NEEDS_RESET` stays set until `Status = 0`. On a corrupt ring the transport raises both the used-buffer and the config-change bits.
- **Why:** after activation the event loop relies on the queue layout validated at `DRIVER_OK`. Linux resets the device before tearing queues down, so it never needs to change them while active. Raising the used-buffer bit too means buffers completed before the corruption was found still get their interrupt.

### D23. virtio-blk request layout and `FLUSH` data
- **Options:** accept any layout (header or status sharing a descriptor with data); fixed layout: header descriptor, data descriptors, status descriptor.
- **Chose:** the plan's fixed layout. A `FLUSH` with data descriptors is rejected (`IOERR`), and so is an `IN`/`OUT` whose data length is not a multiple of 512 (`IOERR`). A zero-length `IN`/`OUT` is accepted as a no-op.
- **Why:** Linux's virtio_blk always uses separate header/data/status descriptors, and the fixed layout keeps the parser small enough to prove in M8. Documented as a limitation.

### D24. AC-4.8 test is not `#[ignore]`d
- **Options:** ignore it like the boot tests; always run it.
- **Chose:** always run it (`cargo test` covers it).
- **Why:** it needs no artifacts; disk validation happens before the kernel is read.

### D25. The vCPU thread's exit reporting
- **Options:** report only on normal returns; also report from a drop guard.
- **Chose:** a drop guard records "vCPU thread ended unexpectedly" if the thread ends without an outcome (e.g. a panic). `VmExit::finish` is first-writer-wins, so the guard is harmless on normal paths. `vcpu::run` gains `GuestStop::Stopped` for "the VMM asked me to stop".
- **Why:** otherwise a panicking vCPU thread would never wake the event loop, and the VMM would hang.

## M5 — virtio-net

### D26. RX flow control
- **Options:** read a frame from the TAP and then look for a chain (and hold or drop the frame if there is none); check for a chain first without consuming it.
- **Chose:** `Queue::has_available` peeks at `avail.idx`. A frame is read from the TAP only when a chain is waiting, so frames queue in the host kernel, not in the VMM. After every RX pass (from an RX notification or a readable TAP), the event loop watches the TAP only if the device is active and the pass did not run out of guest buffers.
- **Why:** this implements D7 without a pending-frame buffer and without dropping frames just because the guest is briefly out of buffers. One rule for both events keeps the level-triggered epoll from spinning.

### D27. The RX virtio-net header from the TAP
- **Options:** pass the TAP's header through unchanged; zero it; set `num_buffers`.
- **Chose:** keep the TAP's header and set `num_buffers` (bytes 10–11) to 1.
- **Why:** the host kernel only fills the first 10 bytes of a 12-byte header, so `num_buffers` would otherwise be stale buffer contents. Without `MRG_RXBUF` the virtio spec requires it to be 1. With offloads off (`TUNSETOFFLOAD 0`) the rest of the header carries no GSO/checksum requests.

### D28. Testing the net device without a TAP
- **Options:** only test the pure parsers; make `Net` generic over its backend.
- **Chose:** `Net<T: Read + Write + Send>`, used with `Tap` in production and a `FakeTap` in unit tests.
- **Why:** the RX starvation, frame-drop and TX-drop paths can be unit-tested without root or a real interface.

### D29. `--guest-mac` default and parser location
- **Options:** clap `default_value` plus `requires = "tap"`; no clap default, with the default applied in `run_boot`.
- **Chose:** `Option<[u8; 6]>` with no clap default; `DEFAULT_MAC` is applied in `run_boot`. The parser `parse_mac` lives in `net.rs` and is wired through clap's `value_parser` in `main.rs`.
- **Why:** a clap default makes `requires` harder to reason about. Keeping the parser in the library makes it unit-testable next to the other net code.

### D30. `struct ifreq`
- **Options:** `libc::ifreq` (union field access); a local 40-byte `#[repr(C)]` struct with just the name and flags.
- **Chose:** the local struct, with unit tests that pin its size, the flags offset and the three ioctl numbers against the Linux header values.
- **Why:** it needs no unsafe union access, and the layout is checked by tests.

### D31. `setup-tap.sh` with a foreign `vmtap0`
- **Options:** delete and recreate it; fail with a message.
- **Chose:** fail and print the `ip link delete` command.
- **Why:** a script run with sudo should not silently delete an interface someone else created.

## M6 — Multiple vCPUs

### D32. CPUID details the plan left open
- **Options:** only set `HTT` when N > 1; also clear it when N = 1. Add missing 0xB/0x1F subleaves; rewrite only the ones KVM reports.
- **Chose:** `HTT` is cleared for N = 1 (a host with HT would otherwise advertise it). A leaf-4 cache counts as shared when its level is ≥ 3. Only the 0xB/0x1F subleaves KVM reports are rewritten; none are added. Raw mode also goes through the topology rewrite, as "vCPU 0 of 1".
- **Why:** the guest sees one consistent "1 socket × N cores" topology for every N, and there is only one CPUID code path.

### D33. Where the vCPU-count check lives
- **Options:** clap only; clap plus a library check.
- **Chose:** clap's `range(1..=4)` (exit 2 on bad input), plus `Error::InvalidVcpus` (exit 2) at the top of `run_boot`.
- **Why:** `run_boot` is a public library function; the MP table and topology are only built for 1..=4 CPUs.

### D34. A vCPU thread fails to start after others are running
- **Options:** return the error at once (leaving running threads unjoined); record it as the VM outcome and go through the normal stop/join path.
- **Chose:** record it with `VmExit::finish`; the event loop returns at once and `stop_all` stops and joins the threads already started.
- **Why:** no vCPU thread is ever left running.

### D35. The 30 s boot bound
- **Options:** leave it unchecked (it is a performance bound, not an AC); check it where the spec's configuration is booted.
- **Chose:** AC-6.4 asserts that each of its 10 boots (disk + TAP + 4 vCPUs) reaches `PHASE1-INIT-OK` within 30 s. Measured: about 4 s each.
- **Why:** AC-6.4 boots exactly the configuration the bound describes, so checking there costs nothing.

## M7 — Hardening

### D36. `TCGETS` in every filter
- **Options:** only `TCSETS` for the signal handler (as AC-7.1 names it); also `TCGETS`.
- **Chose:** `ioctl` allows `TCGETS` and `TCSETS` on every thread, plus `KVM_RUN` on vCPUs.
- **Why:** the traces show glibc's `tcsetattr` issues `TCGETS`, `TCSETS`, `TCGETS`. With `TCSETS` alone, a signal handler would kill the VMM with `SIGSYS` while restoring the terminal. AC-7.1 lists a minimum, so allowing more is within the spec.

### D37. Allocator syscalls
- **Options:** allow only the allocator syscalls each trace happened to show; allow the whole family on every thread.
- **Chose:** `mmap`, `mprotect`, `munmap`, `madvise`, `brk` and `mremap` on every thread. `mmap`/`mprotect` are argument-filtered so `PROT_EXEC` is never allowed.
- **Why:** which of these malloc calls depends on heap state (arena growth, large blocks, trimming), not on the code path. A trace-exact list would kill the VMM at random under memory pressure (pitfall 9). Forbidding `PROT_EXEC` keeps a compromised thread from creating executable memory.

### D38. Seccomp only in `boot` mode
- **Options:** filter `raw` mode too; `boot` only.
- **Chose:** `boot` only.
- **Why:** AC-7.1 names `boot`'s threads (main/event loop, vCPUs, stdin). `raw` runs its single vCPU on the caller's thread, and the M1/M2 tests call `run_raw` inside the test harness process, which a filter would then restrict or kill.

### D39. A filter fails to install
- **Options:** keep running unfiltered with a warning; end the VM.
- **Chose:** the thread records `Error::Seccomp` (exit 1) as the VM outcome and does no work; the normal stop/join path follows.
- **Why:** a VMM that silently runs without the filter it was asked to use is worse than one that refuses to run.

### D40. How the verify script checks AC-7.2 and AC-7.3
- **Options:** only `grep` the source; also check at run time. For AC-7.3: rerun every M4–M6 test; reuse the M4–M6 results from the same run.
- **Chose:** AC-7.2 is a source-order check in `run_boot` plus the run-time `NoNewPrivs: 1` check on every thread. AC-7.3 passes when every AC-4.x/5.x/6.x check in the same run passed (they run with seccomp, the default) and no M4–M6 test passes `--no-seccomp`.
- **Why:** the run-time check proves the flag is really set. Rerunning four minutes of boots would prove nothing new.

### D41. Kernel worker threads in the VMM process
- **Options:** require exactly the VMM's threads; also allow KVM's own workers.
- **Chose:** the AC-7.1 test requires `vmm`, `stdin` and every `vcpuN`, allows only extra threads named `kvm-*` (KVM's `kvm-nx-lpage-recovery` worker), and requires `Seccomp: 2` on every thread, workers included.
- **Why:** KVM adds that worker to the VM owner's process. It is not a VMM thread (the spec's scope), and it inherits the filter anyway.

## M8 — Verification

### D42. What "a chain that loops back on itself" means under concurrent rewrites
- **Options:** make `walk_chain` track visited indices and reject any revisit; keep the spec's count-based definition and prove the loop property for a stable table.
- **Chose:** keep `walk_chain` count-based (reject once a chain would exceed `size` descriptors, as the spec's behavioral rules define a loop). `walk_chain_bounded` proves termination, in-table indices, the size bound and `INDIRECT` rejection for arbitrary (even concurrently rewritten) descriptors, and proves that a revisit is rejected when the table is stable (`KaniMem.stable`).
- **Why:** Kani's counterexample (descriptor 0 → 0, rewritten between reads so the second copy ends the chain) is bounded and safe: each descriptor is read once into a local copy and checked. A visited set would add state for no safety gain, and would change behavior beyond the spec. With the user's approval, AC-8.2(d) was reworded to state exactly these two properties (commit `7f2dd3d`).

### D43. Chains of concrete length in the harnesses
- **Options:** one symbolic-length `Vec` (`all[..n].to_vec()`); branch on `n` and pass slices of each concrete length 0..=4.
- **Chose:** `for_any_chain` branches over each concrete length.
- **Why:** identical coverage, but with symbolic-length heap data `net_tx_bounds` alone ran past 15 minutes. Now all seven harnesses take about a minute, well inside the 20-minute bound, with no need for Q5's fallback to queue size 2.

### D44. `walk_chain` loop shape
- **Options:** check the length at the top of each iteration (the body runs `size + 1` times); check it after adding a descriptor that still has `NEXT`.
- **Chose:** the check after adding. Behavior is unchanged (a chain that still has `NEXT` after `size` descriptors is `TooLong`).
- **Why:** the body then runs at most `size` times, so Kani proves termination with the spec's unwind bound of 5 for queue size 4.

### D45. Harness bookkeeping in fixed-size arrays
- **Options:** record descriptor reads in a `Vec`; in a `[u64; 4]` with a counter.
- **Chose:** the fixed array, with an assertion at record time that there are never more than 4 descriptor reads.
- **Why:** harness loops over a `Vec` failed Kani's unwinding assertions (an `assert!` on its length doesn't bound the loop for the unwinder). Fixed bounds keep the proof about the code, not about the harness.

### D46. `fuzz/Cargo.lock`
- **Options:** commit it; ignore it.
- **Chose:** ignore it (`.gitignore`, next to `fuzz/target` and `fuzz/artifacts`).
- **Why:** committing it would add a file outside the spec's output-file list, which needs a spec change first. The root `Cargo.lock` still pins every shared crate version.

### D47. Proof and fuzz checks in the verify script
- **Options:** trust the exit code of `cargo kani`; check each named harness. For the fuzzer: use `git status` to detect corpus changes; compare checksums.
- **Chose:** AC-8.2 requires exit 0 within 1200 s, `VERIFICATION:- SUCCESSFUL` for each of the seven named harnesses, and exactly seven successes. AC-8.3 compares the seed corpus's checksums before and after fuzzing a `mktemp -d` copy. AC-P1 checks that every `**AC-…**` in the spec has a `check` line. AC-P2 checks the required notes sections (≥ 50 words each), the per-milestone explanations, the "not cover" list and ≥ 12 answered questions.
- **Why:** a harness that is renamed or silently dropped must not pass, and the corpus check must work whether or not the corpus is committed yet.

### D48. Seccomp run-time checks on a host that already filters every process
- **Question:** after a WSL2 update (kernel 6.18), `/init` and therefore every process runs with an inherited seccomp filter (`Seccomp: 2`, `Seccomp_filters: 1`). AC-7.5's test asserted `Seccomp: 0` on every thread under `--no-seccomp` and failed; AC-7.1's test asserted `Seccomp: 2`, which now passes even with no VMM filter.
- **Options:** require a host without seccomp; compare the mode; compare `Seccomp_filters` with the count the test process inherited.
- **Chose:** count filters. AC-7.1: every thread has more filters than the test process inherited. AC-7.5: every thread has exactly as many. The spec text (AC-7.1, AC-7.5) is unchanged; only the tests' measurement changed.
- **Why:** it measures what the VMM itself installed, so both tests are right on hosts with and without an inherited filter, and AC-7.1 can no longer pass vacuously.

### D49. `failed_ids` loop variable in the Phase 2 verify script
- **Question:** `m4_to_m6_with_seccomp` (AC-7.3) looped with a non-`local` `id`, which overwrote `check`'s `id`, so a failing AC-7.3 was printed and recorded under the last failed M4-M6 id.
- **Chose:** declare `id` local in that function.
- **Why:** a one-word fix; the PASS/FAIL line and log path must name the criterion that ran.
