#!/usr/bin/env bash
# Runs the Phase 1 acceptance criteria and prints one PASS/FAIL line per criterion.
# Exits 0 only if every criterion passes. Logs go to target/verify/<id>.log.
set -uo pipefail
cd "$(dirname "$0")/.."

LOG_DIR=target/verify
VMM=target/debug/vmm
mkdir -p "$LOG_DIR"
failed=0

# check <id> <description> <command...>
check() {
    local id=$1 desc=$2
    shift 2
    if "$@" >"$LOG_DIR/$id.log" 2>&1; then
        printf 'PASS  %-7s %s\n' "$id" "$desc"
    else
        printf 'FAIL  %-7s %s (see %s)\n' "$id" "$desc" "$LOG_DIR/$id.log"
        failed=1
    fi
}

# Every `unsafe {` / `unsafe impl` in src/ has a `SAFETY:` comment within the 3 lines above it.
safety_comments() {
    local status=0 file
    while IFS= read -r file; do
        awk -v f="$file" '
            /^[[:space:]]*\/\// { buf[NR] = $0; next }
            /unsafe[[:space:]]*(\{|impl)/ {
                ok = 0
                for (i = NR - 3; i < NR; i++) if (buf[i] ~ /SAFETY:/) ok = 1
                if (!ok) { printf "%s:%d: unsafe without SAFETY comment\n", f, NR; bad = 1 }
            }
            { buf[NR] = $0 }
            END { exit bad }
        ' "$file" || status=1
    done < <(find src -name '*.rs')
    return $status
}

missing_kvm_device() {
    local code=0
    "$VMM" --kvm-device /nonexistent/kvm raw --image guests/build/hello16.bin --mode real \
        >/dev/null 2>"$LOG_DIR/ac-0.4.stderr" || code=$?
    cat "$LOG_DIR/ac-0.4.stderr"
    echo "exit code: $code"
    [ "$code" -eq 2 ] && grep -q '/nonexistent/kvm' "$LOG_DIR/ac-0.4.stderr"
}

real_mode_hello() {
    local code=0
    timeout 10 "$VMM" raw --image guests/build/hello16.bin --mode real \
        >"$LOG_DIR/ac-1.2.stdout" || code=$?
    echo "exit code: $code"
    [ "$code" -eq 0 ] && cmp "$LOG_DIR/ac-1.2.stdout" <(printf 'Hello from real mode!\n')
}

long_mode_source() {
    grep -Eq '^[[:space:]]*bits[[:space:]]+64' guests/hello64.asm \
        && grep -Eiq '^[^;]*\<r(8|9|1[0-5])[bwd]?\>' guests/hello64.asm \
        && nasm -f bin -o "$LOG_DIR/hello64.bin" guests/hello64.asm
}

long_mode_hello() {
    local code=0
    timeout 10 "$VMM" raw --image guests/build/hello64.bin --mode long \
        >"$LOG_DIR/ac-2.2.stdout" || code=$?
    echo "exit code: $code"
    [ "$code" -eq 0 ] && cmp "$LOG_DIR/ac-2.2.stdout" <(printf 'Hello from long mode!\n')
}

kernel_is_6_12() {
    file artifacts/bzImage
    file artifacts/bzImage | grep -q 'bzImage, version 6\.12\.'
}

initramfs_contract() {
    local init
    init=$(cpio --quiet -i --to-stdout init < artifacts/initramfs.cpio) || return 1
    echo "$init"
    for mount in proc sysfs devtmpfs; do
        grep -q "mount -t $mount" <<<"$init" || { echo "does not mount $mount"; return 1; }
    done
    grep -q 'echo PHASE1-INIT-OK' <<<"$init" \
        && grep -q "export PS1='phase1# '" <<<"$init" \
        && grep -q 'exec setsid cttyhack /bin/sh' <<<"$init" \
        && cpio --quiet -it < artifacts/initramfs.cpio | grep -qx 'bin/busybox'
}

# Words in the part of notes/phase-1.md under heading $1 (up to the next heading of
# the same or higher level). With $2, heading $1 only counts inside section $2.
notes_section_words() {
    awk -v h="$1" -v parent="${2:-}" '
        parent != "" && index($0, parent) == 1 { in_parent = 1; next }
        parent != "" && /^## / { in_parent = 0 }
        on && /^#+ / && index($0, " ") <= level { on = 0 }
        (parent == "" || in_parent) && index($0, h) == 1 { on = 1; level = index($0, " "); next }
        on { print }
    ' notes/phase-1.md | wc -w
}

notes_complete() {
    local ok=0 words heading answered
    [ -f notes/phase-1.md ] || { echo "notes/phase-1.md missing"; return 1; }
    for heading in "## What each milestone does" "## VM-exit handling flow"; do
        words=$(notes_section_words "$heading")
        echo "$heading: $words words"
        [ "$words" -ge 50 ] || { echo "  -> too short or missing (need >= 50 words)"; ok=1; }
    done
    for heading in "### M1" "### M2" "### M3"; do
        words=$(notes_section_words "$heading" "## What each milestone does")
        echo "$heading explanation: $words words"
        [ "$words" -ge 50 ] || { echo "  -> too short or missing (need >= 50 words)"; ok=1; }
    done
    # A question counts only if the next non-blank line is an answer, not another
    # question or a heading.
    answered=$(awk '
        /^\*\*Q[0-9]+\./ { pending = 1; next }
        pending && NF { if ($0 !~ /^(\*\*Q[0-9]+\.|#)/) n++; pending = 0 }
        END { print n + 0 }
    ' notes/phase-1.md)
    echo "answered interview questions: $answered"
    [ "$answered" -ge 10 ] || { echo "  -> need at least 10"; ok=1; }
    return $ok
}

# Setup: build the VMM and the raw guests (their failures surface in the checks below).
cargo build >"$LOG_DIR/build.log" 2>&1 || echo "note: cargo build failed (see $LOG_DIR/build.log)"
scripts/build-guests.sh >"$LOG_DIR/build-guests.log" 2>&1 || echo "note: build-guests.sh failed (see $LOG_DIR/build-guests.log)"
# The kernel takes many minutes to build, so only build artifacts that are missing.
if [ ! -f artifacts/bzImage ]; then
    echo "building artifacts/bzImage (slow; log: $LOG_DIR/build-kernel.log)"
    scripts/build-kernel.sh >"$LOG_DIR/build-kernel.log" 2>&1 || echo "note: build-kernel.sh failed"
fi
if [ ! -f artifacts/initramfs.cpio ]; then
    scripts/build-initramfs.sh >"$LOG_DIR/build-initramfs.log" 2>&1 || echo "note: build-initramfs.sh failed"
fi

echo "== Always =="
check AC-0.1 "cargo fmt --check" cargo fmt --check
check AC-0.2 "cargo clippy --all-targets -- -D warnings" cargo clippy --all-targets -- -D warnings
check AC-0.3 "every unsafe block has a SAFETY comment" safety_comments
check AC-0.4 "unopenable KVM device -> exit 2, path in error" missing_kvm_device

echo "== M1 — Real-mode hello =="
check AC-1.1 "hello16.asm assembles with nasm -f bin" nasm -f bin -o "$LOG_DIR/hello16.bin" guests/hello16.asm
check AC-1.2 "raw real-mode guest prints greeting, exits 0" real_mode_hello
check AC-1.3 "cargo test --test m1_real_mode" cargo test --test m1_real_mode

echo "== M2 — Long-mode hello =="
check AC-2.1 "hello64.asm is bits 64 and uses r8-r15" long_mode_source
check AC-2.2 "raw long-mode guest prints greeting, exits 0" long_mode_hello
check AC-2.3 "EFER.LMA = 1 and CS.L = 1 after halt" cargo test --test m2_long_mode vcpu_is_in_long_mode_after_halt
check AC-2.4 "cargo test --test m2_long_mode" cargo test --test m2_long_mode
check AC-2.5 "M1 tests still pass" cargo test --test m1_real_mode

echo "== M3 — Linux boot =="
m3() { cargo test --test m3_linux_boot -- --ignored --exact "$1"; }
check AC-3.1 "artifacts/bzImage is Linux 6.12" kernel_is_6_12
check AC-3.2 "artifacts/initramfs.cpio meets the /init contract" initramfs_contract
check AC-3.3 "PHASE1-INIT-OK within 30 s" m3 boot_prints_init_marker_within_30s
check AC-3.4 "prompt appears; echo vmm-\$((6*7)) prints vmm-42" m3 shell_receives_keyboard_input
check AC-3.5 "uname -r starts with 6.12" m3 uname_reports_linux_6_12
check AC-3.6 "reboot -f exits 0 within 10 s, terminal restored" m3 reboot_exits_zero_and_restores_terminal
check AC-3.7 "cargo test --test m3_linux_boot -- --ignored" cargo test --test m3_linux_boot -- --ignored
check AC-3.8 "M1 and M2 tests still pass" cargo test --test m1_real_mode --test m2_long_mode

echo "== Phase 1 done =="
check AC-P2 "notes/phase-1.md: milestones, VM-exit flow, >= 10 answered questions" notes_complete

if [ "$failed" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$failed"
