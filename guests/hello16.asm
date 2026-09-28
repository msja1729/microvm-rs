; M1 raw guest: print a string on COM1 from 16-bit real mode, then halt.
; The VMM loads this image at guest-physical 0x1000 with all segment bases 0.
bits 16
org 0x1000

start:
    cld                     ; lodsb walks forward through the string
    mov dx, 0x3f8           ; COM1 transmit register
    mov si, msg             ; DS:SI -> msg (DS = 0)
.next:
    lodsb                   ; al = [ds:si], si++
    test al, al
    jz .done                ; stop at the NUL terminator
    out dx, al              ; each OUT causes a KVM_EXIT_IO the VMM forwards to stdout
    jmp .next
.done:
    cli
    hlt                     ; KVM_EXIT_HLT -> VMM exits with code 0

msg: db "Hello from real mode!", 10, 0
