; M2 raw guest: print a string on COM1 from 64-bit long mode, then halt.
; The VMM enters long mode itself; this code starts at 0x1000 with paging on and
; the first 1 GiB identity-mapped.
bits 64
org 0x1000

start:
    lea r8, [rel msg]       ; r8 only exists in 64-bit mode: in any other mode the
                            ; REX prefix decodes as a different instruction
    mov dx, 0x3f8           ; COM1 transmit register
.next:
    mov al, [r8]
    test al, al
    jz .done                ; stop at the NUL terminator
    out dx, al
    inc r8
    jmp .next
.done:
    cli
    hlt                     ; KVM_EXIT_HLT -> VMM exits with code 0

msg: db "Hello from long mode!", 10, 0
