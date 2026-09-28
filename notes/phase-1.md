# Phase 1 — Learning notes

## What each milestone does

### M1 — Real-mode hello
A virtual machine under KVM starts as almost nothing: no memory, no CPU, no devices. The VMM opens `/dev/kvm`, asks for a new VM, allocates 16 MiB of ordinary process memory and tells KVM "this is the guest's physical RAM, starting at address 0". It copies a tiny hand-written program (`hello16.bin`) to address `0x1000` in that RAM and creates one virtual CPU. A freshly created x86 CPU is in 16-bit *real mode*, the mode PCs boot in, and would start executing at the top of the first megabyte where a BIOS normally lives. There is no BIOS here, so the VMM rewrites the CPU's registers so it starts at `0x1000` instead.

Then the VMM repeatedly calls `KVM_RUN`. The guest runs at full hardware speed until it does something the hardware can't finish on its own. Each time it writes a character to the serial port (`out` to port `0x3f8`), the CPU stops and control comes back to the VMM with the port number and the byte (a *VM exit*). The VMM hands the byte to its emulated 16550 UART, which writes it to stdout, and calls `KVM_RUN` again. When the program executes `HLT` ("stop the CPU"), KVM returns that too, and the VMM exits with code 0. That only works because raw mode creates no in-kernel interrupt controller; with one, KVM would handle `HLT` itself by waiting for an interrupt that never comes.

### M2 — Long-mode hello
Same idea, but the guest program is 64-bit code, which only runs in *long mode*. A real CPU gets there in steps: real mode → protected mode → paging on → long mode, each step done by code running in the previous mode. The VMM skips all that and writes the end state directly. It builds three things in guest memory: a GDT (the table that describes code and data segments; its 64-bit code segment is what makes instructions decode as 64-bit), an empty IDT, and page tables that map the first 1 GiB of addresses one-to-one onto physical memory. It then sets the control registers (protected mode + paging on, PAE on, long mode enabled in EFER) and the segment registers with `KVM_SET_SREGS`, so the very first instruction the guest runs is already 64-bit. The VMM also gives the vCPU the host's CPUID feature list, because KVM refuses to enable long mode for a CPU that doesn't claim to support it.

The guest uses `r8`, a register that only exists in 64-bit mode, so it prints its greeting only if long mode really works. As proof, the library returns the CPU's final special registers, and the test checks EFER.LMA ("long mode active") and CS.L ("64-bit code segment").

### M3 — Linux boot
Now the guest is an unmodified Linux kernel. The VMM does the job of a boot loader: `linux-loader` copies the kernel part of the bzImage to 1 MiB, the initramfs (a cpio archive holding BusyBox and an `/init` script) goes at the top of RAM, and the command line goes at `0x20000`. The *zero page* (`boot_params`) at `0x7000` tells the kernel where RAM is (an e820 memory map), where the command line and initramfs are, and which loader booted it. The vCPU starts in long mode exactly as in M2, at the kernel's 64-bit entry point with RSI pointing to the zero page, as the Linux 64-bit boot protocol requires.

A real OS needs more hardware than a raw program. KVM itself emulates the interrupt controllers (PIC, IOAPIC, local APIC) and the timer (PIT), because they are touched constantly and must be fast. The VMM emulates only the serial port and a sliver of the i8042 keyboard controller. For input, a separate thread reads the host terminal and pushes keystrokes into the UART's receive FIFO, then signals an eventfd that KVM turns into IRQ 4 inside the guest, so Linux's serial driver wakes up and reads them. The host terminal is switched to raw mode so every key, including Ctrl-C, reaches the guest, and it is always restored when the VMM exits. `/init` mounts the pseudo-filesystems, prints `PHASE1-INIT-OK` and starts a shell. When the user runs `reboot -f`, Linux writes the reset command `0xFE` to i8042 port `0x64`, and the VMM treats that as "power off" and exits with 0.

## VM-exit handling flow

Every guest action the hardware cannot complete by itself ends `KVM_RUN` with an exit reason. The whole VMM is built around one loop, `vcpu::run` in `src/vcpu.rs`:

```
run_raw / run_boot
  └─ set up VM, memory, (irqchip + PIT: boot only), vCPU registers, devices
     └─ vcpu::run loop:
          KVM_RUN ──┬─ Err(EINTR | EAGAIN) ─────────► retry KVM_RUN
                    ├─ Err(other) ──────────────────► Error::Kvm            → exit 1
                    ├─ IoOut(port, data) ──► PioBus::write
                    │     ├─ port claimed by a device → device.write
                    │     │     └─ i8042 0x64 ← 0xFE → PioAction::Reset → GuestStop::Reset
                    │     └─ unclaimed → ignore (debug log)
                    ├─ IoIn(port, data) ───► PioBus::read
                    │     ├─ port claimed → device fills `data`
                    │     └─ unclaimed → fill with 0xFF (debug log)
                    ├─ MmioRead ─────────────────────► fill with 0xFF
                    ├─ MmioWrite ────────────────────► ignore
                    ├─ Hlt ──────────────────────────► GuestStop::Halted
                    ├─ Shutdown ─────────────────────► GuestStop::Shutdown
                    └─ anything else ────────────────► Error::UnexpectedExit → exit 1
```

After a handled exit the loop calls `KVM_RUN` again; KVM has already advanced RIP past the `IN`/`OUT`, and for an `IN` it copies `data` into the guest's register on re-entry. Devices on the bus: COM1 at `0x3f8`–`0x3ff` (both modes) and the i8042 at `0x60`–`0x64` (boot only).

How each mode turns a stop into an exit code:

| Stop | `raw` | `boot` |
|---|---|---|
| `Halted` | exit 0, final special registers returned to the caller | exit 1 (impossible with an in-kernel irqchip) |
| `Shutdown` (triple fault) | exit 1: always a bug in raw setup | exit 0: Linux's last-resort reset |
| `Reset` (i8042) | exit 1 (no i8042 attached) | exit 0 |

What never reaches this loop in `boot` mode, because KVM handles it in the kernel: accesses to the PIC, PIT and IOAPIC ports and MMIO, local-APIC MMIO, `HLT` (the vCPU sleeps inside `KVM_RUN` until an interrupt arrives), and IRQ 4 injection through the irqfd. So `KVM_RUN` can block for a long time while the guest is idle, and why host input needs its own thread.

Guest-controlled values can't crash this loop: port and offset arithmetic is checked, device register indexes are range-checked, unclaimed accesses have defined results, and the crate denies `unwrap`/`expect`. Every failure becomes an `Error` with an exit code.

## Interview questions

### M1 — Real-mode hello

**Q1. Why does creating KVM's in-kernel irqchip stop `KVM_EXIT_HLT` from reaching the VMM, and what would a raw guest's `cli; hlt` do then?**
With an in-kernel LAPIC, KVM emulates `HLT` itself: the vCPU thread sleeps inside `KVM_RUN` until the in-kernel interrupt controller has an interrupt to deliver, because waking a halted CPU is the interrupt controller's job. User space is never told. With `cli` first, no maskable interrupt can ever wake it, so `KVM_RUN` blocks forever and the VMM hangs. That is why `raw` mode creates no irqchip: then KVM has nothing to wait on and returns `KVM_EXIT_HLT`.

**Q2. At reset x86 has CS selector `0xF000`, base `0xFFFF0000`, IP `0xFFF0`. What did the VMM change so the first fetch is at `0x1000`, and why is there no BIOS to jump to?**
The VMM reads the reset-state special registers (`KVM_GET_SREGS`), sets every segment's selector and base to 0, writes them back (`KVM_SET_SREGS`), then sets RIP = `0x1000` with `KVM_SET_REGS`. CR0.PE stays clear, so the CPU is still in real mode and the linear address is `CS.base + IP = 0x1000`. There is no BIOS because the VMM never loads firmware. Guest memory is just the anonymous mapping we registered, and nothing lives at `0xFFFF0` (with 16 MiB of RAM that address is plain zeroed RAM). The VMM does the firmware's job directly: put code in memory and point the CPU at it.

**Q3. Walk through one `out dx, al` from `hello16.asm`: what KVM hands the VMM, how it reaches the serial device, and what the next `KVM_RUN` does.**
The `OUT` to port `0x3f8` causes a VM exit. KVM decodes it and returns from `KVM_RUN` with `exit_reason = KVM_EXIT_IO`, direction out, port `0x3f8`, size 1, and the data byte in the shared `kvm_run` page. `kvm-ioctls` presents this as `VcpuExit::IoOut(0x3f8, &[byte])`. The VMM's `PioBus` finds COM1 (ports `0x3f8`–`0x3ff`) and calls the device with offset 0. With DLAB clear, the vm-superio 16550 treats offset 0 as the transmit register and writes the byte to stdout. The next `KVM_RUN` resumes the guest at the instruction after the `OUT`: KVM already advanced RIP when it handled the exit, so the VMM does not touch any registers.

### M2 — Long-mode hello

**Q1. The VMM puts the vCPU in long mode without the guest ever running in real or protected mode. What exactly must be set, and why does the order of the steps a real CPU performs not matter here?**
Long mode needs protected mode on (CR0.PE), PAE paging (CR4.PAE), a PML4 in CR3, EFER.LME, and paging on (CR0.PG). When those hold, the CPU sets EFER.LMA. On bare metal these must be enabled in a strict order, because each instruction runs in the mode the previous one left behind. Through `KVM_SET_SREGS` the VMM writes the *final* state atomically: KVM checks that the combination is consistent (e.g. LMA requires LME and PG, and CS.L=1 with D=0) and loads it. The vCPU's first instruction is already 64-bit code. The VMM also sets EFER.LMA itself, since it is describing the state "as if" the switch had just happened.

**Q2. Why does `KVM_SET_SREGS` need both the GDT in guest memory *and* fully decoded segment registers? What breaks if they disagree?**
A segment register has a hidden part (base, limit, type, L/D bits) that the CPU fills from the GDT only when a selector is *loaded* (e.g. `mov ds, ax`, a far jump, `iretq`). `KVM_SET_SREGS` writes the hidden part directly and never reads the GDT, so it is what the vCPU uses right away. The in-memory GDT only matters later, when the guest reloads a selector (Linux does this early). If they disagree, the guest runs correctly at first, then changes behavior or triple-faults at its first segment reload. That is hard to debug, so the VMM builds both from the same descriptor values (`gdt::table()` → `gdt::segment()`).

**Q3. The identity map uses 2 MiB pages. Walk the translation of guest-virtual `0x1000`, and say why the VMM didn't use 1 GiB pages.**
With CR3 = `0x9000`, bits 47–39 of `0x1000` (0) select PML4[0] → the PDPT at `0xA000`. Bits 38–30 (0) select PDPT[0] → the PD at `0xB000`. Bits 29–21 (0) select PD[0], which has the PS bit set, so it maps a whole 2 MiB page at physical 0, and the low 21 bits (`0x1000`) are the offset: physical `0x1000`. 512 PD entries cover 1 GiB with three tables. 1 GiB pages would need one table fewer, but they require the `pdpe1gb` CPUID feature. That is not guaranteed to be passed through under nested virtualization, and an unsupported large page causes a page fault → triple fault with no clue why.

### M3 — Linux boot

**Q1. The shell prints output fine without interrupts, so why does keyboard input hang if the serial IRQ is never delivered, and how does a keystroke get from the host into the guest in this VMM?**
Output works by polling: the guest writes the transmit register and each write traps to the VMM, so no interrupt is needed. Input is the reverse direction, and Linux's 8250 driver does not poll for it; it waits for the UART's "received data available" interrupt. The path here is: the stdin thread `read`s the keystroke → locks the shared UART and `enqueue_raw_bytes` puts it in the 64-byte RX FIFO and sets the data-ready bit → vm-superio calls the trigger, which writes to an eventfd → that eventfd is registered with `KVM_IRQFD` for GSI 4, so KVM raises IRQ 4 in the in-kernel PIC/IOAPIC and injects it into the vCPU, even while the vCPU is halted inside `KVM_RUN` → the guest's IRQ handler reads the receive register (a port read that exits to the VMM, which pops the byte from the FIFO). Without the irqfd the byte sits in the FIFO forever.

**Q2. Why does the VMM enter the kernel at `load address + 0x200` in 64-bit mode instead of the bzImage's 16-bit real-mode entry, and what does the kernel expect at that moment?**
The 16-bit entry is for BIOS boot: its setup code calls BIOS services (memory map via `int 0x15`, video) that don't exist here. The 64-bit boot protocol skips all of that: the loader has already done the setup code's work (the e820 map, command line and initrd location in `boot_params`). At `startup_64` (offset 0x200 in the protected-mode kernel) the kernel expects: 64-bit mode with paging, an identity mapping covering the kernel, `boot_params` and the command line; CS = `__BOOT_CS` (0x10) and data segments = `__BOOT_DS` (0x18) in a GDT; interrupts disabled; RSI = address of `boot_params`. That is exactly M2's setup plus RSI, which is why M2 came first.

**Q3. Linux reboots with `reboot -f` and the VMM exits with 0. Trace that path, and explain why a triple fault also counts as a clean exit in `boot` mode but not in `raw` mode.**
With `reboot=k` on the command line, the kernel's reboot path uses the keyboard controller: it writes 0xFE ("pulse the CPU reset line") to i8042 port 0x64. That `OUT` exits to the VMM as `KVM_EXIT_IO`, the i8042 device returns `PioAction::Reset`, the run loop returns `GuestStop::Reset`, `run_boot` returns `Ok`, the terminal guard restores the terminal, and `main` exits 0. If that reset didn't work, Linux's fallback is a deliberate triple fault, which KVM reports as `KVM_EXIT_SHUTDOWN`. So in `boot` mode SHUTDOWN is a normal way for the guest to say "reset me", and it also maps to 0. A raw test guest never asks to reset, so a SHUTDOWN there can only mean broken GDT/page tables, which is why `raw` mode reports it as exit 1.

### Phase 1 overall

**Q1. The spec says "a guest is untrusted: nothing a guest does may panic the VMM". Which guest inputs reach VMM code in Phase 1, and how is each one made safe?**
Only VM exits reach VMM code, and the guest controls their contents: the port number, the access size, the data bytes, and MMIO addresses. Port lookup uses `checked_sub` and a range check, so no port value can index outside a device. The UART maps each byte to a register with checked addition and ignores anything past its 8 registers. Unclaimed ports and MMIO get defined answers (all-ones reads, ignored writes) instead of errors, because Linux probes lots of absent hardware. A reset request is just a return value. Any exit reason the VMM doesn't expect becomes an `Error` (exit 1), not a panic. The library and binary `#![deny(clippy::unwrap_used, clippy::expect_used)]`, so a lurking `unwrap` can't pass `clippy -D warnings`. Even a poisoned mutex becomes an exit-1 error. The guest can still make the VMM *exit*, for example by triple-faulting, but always through a controlled, documented exit code.

**Q2. `KVM_RUN` blocks the vCPU thread. Why does that force host input onto a separate thread, and what would it take to do it single-threaded?**
While the guest is idle, Linux executes `HLT`. With an in-kernel irqchip, KVM parks the vCPU thread *inside* `KVM_RUN` until an interrupt is pending, possibly for minutes. A single thread doing `KVM_RUN` then `epoll` on stdin would never reach the `epoll` while the guest waits for a keystroke: a deadlock, since the keystroke is exactly what it waits for. With a second thread, stdin is read independently, the byte goes into the UART FIFO under a mutex, and the eventfd → `KVM_IRQFD` path injects IRQ 4 directly into the halted vCPU without the vCPU thread's help. A single-threaded design would need a way to kick the vCPU out of `KVM_RUN` (a signal to that thread plus `immediate_exit` in `kvm_run`), then poll stdin, then re-enter. That's more moving parts for no benefit with one vCPU.
