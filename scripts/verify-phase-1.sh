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

# Setup: build the VMM and the raw guests (their failures surface in the checks below).
cargo build >"$LOG_DIR/build.log" 2>&1 || echo "note: cargo build failed (see $LOG_DIR/build.log)"
scripts/build-guests.sh >"$LOG_DIR/build-guests.log" 2>&1 || echo "note: build-guests.sh failed (see $LOG_DIR/build-guests.log)"

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

if [ "$failed" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$failed"
