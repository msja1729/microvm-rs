# Phase 1 — Learning notes

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
