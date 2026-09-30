#!/usr/bin/env bash
# Runs the Phase 2 acceptance criteria and prints one PASS/FAIL line per criterion.
# Exits 0 only if every criterion passes. Logs go to target/verify-phase-2/<id>.log.
# Never uses sudo.
set -uo pipefail
cd "$(dirname "$0")/.."

LOG_DIR=target/verify-phase-2
VMM=target/debug/vmm
mkdir -p "$LOG_DIR"
failed=0
# IDs of failed (or skipped) criteria, for criteria that depend on earlier ones.
failed_ids=""

# check <id> <description> <command...>
check() {
    local id=$1 desc=$2
    shift 2
    if "$@" >"$LOG_DIR/$id.log" 2>&1; then
        printf 'PASS  %-7s %s\n' "$id" "$desc"
    else
        printf 'FAIL  %-7s %s (see %s)\n' "$id" "$desc" "$LOG_DIR/$id.log"
        failed=1
        failed_ids="$failed_ids $id"
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
    "$VMM" --kvm-device /nonexistent/kvm boot --kernel artifacts/bzImage \
        >/dev/null 2>"$LOG_DIR/ac-0.4.stderr" || code=$?
    cat "$LOG_DIR/ac-0.4.stderr"
    echo "exit code: $code"
    [ "$code" -eq 2 ] && grep -q '/nonexistent/kvm' "$LOG_DIR/ac-0.4.stderr"
}

# AC-0.5: Phase 1 still passes, on the Phase 2 kernel (build-kernel.sh is a no-op if
# artifacts/bzImage already matches the version and configs/phase2.config).
phase1_regression() {
    scripts/build-kernel.sh && scripts/verify-phase-1.sh
}

kernel_config() {
    local opt ok=0
    for opt in VIRTIO_MMIO VIRTIO_MMIO_CMDLINE_DEVICES VIRTIO_BLK EXT4_FS X86_MPPARSE "$@"; do
        if grep -qx "CONFIG_$opt=y" artifacts/kernel.config; then
            echo "CONFIG_$opt=y"
        else
            echo "CONFIG_$opt is not =y"; ok=1
        fi
    done
    return $ok
}

disk_image() {
    [ "$(id -u)" -ne 0 ] || { echo "must run without root"; return 1; }
    scripts/build-disk.sh || return 1
    file artifacts/disk.ext4
    file artifacts/disk.ext4 | grep -q 'ext4 filesystem' || return 1
    [ "$(stat -c %s artifacts/disk.ext4)" -eq $((64 << 20)) ] || { echo "not 64 MiB"; return 1; }
    [ "$(debugfs -R 'cat /hello.txt' artifacts/disk.ext4 2>/dev/null)" = phase2-disk ]
}

# unit_tests <n> <exact test names...>: run them and require that exactly n ran and passed
# (a renamed test would otherwise make the filter match nothing and "pass").
unit_tests() {
    local n=$1 out
    shift
    out=$(cargo test --lib -- --exact "$@" 2>&1)
    echo "$out"
    grep -q "test result: ok. $n passed" <<<"$out"
}

# The TAP precondition: vmtap0 exists, is owned by us, and has 172.16.0.1/24.
tap_precondition() {
    [ -e /sys/class/net/vmtap0 ] \
        && [ "$(cat /sys/class/net/vmtap0/owner 2>/dev/null)" = "$(id -u)" ] \
        && ip -4 addr show vmtap0 2>/dev/null | grep -q 'inet 172\.16\.0\.1/24'
}

# tap_check <id> <description> <command...>: like `check`, but without the TAP
# precondition it prints SKIP and counts as a failure.
tap_check() {
    if tap_precondition; then
        check "$@"
    else
        printf 'SKIP  %-7s %s (run: sudo scripts/setup-tap.sh)\n' "$1" "$2"
        failed=1
        failed_ids="$failed_ids $1"
    fi
}

# Every test in the M4 and M5 binaries, rerun with the given vCPU count.
m4_m5_with_vcpus() {
    VMM_TEST_VCPUS=$1 cargo test --test m4_virtio_blk --test m5_virtio_net \
        -- --include-ignored --test-threads=1
}

# AC-7.1 (static part): filters are defined only in src/seccomp.rs, and every
# allowlist line there (`(libc::SYS_...`) carries a `// why` comment.
seccomp_source() {
    local ok=0 outside uncommented
    outside=$(grep -rnE 'SeccompFilter|apply_filter|libc::SYS_' src --include='*.rs' | grep -v '^src/seccomp.rs:')
    [ -z "$outside" ] || { echo "seccomp rules outside src/seccomp.rs:"; echo "$outside"; ok=1; }
    uncommented=$(grep -nE '^[[:space:]]*\(libc::SYS_' src/seccomp.rs | grep -v '//')
    [ -z "$uncommented" ] || { echo "allowlist lines without a comment:"; echo "$uncommented"; ok=1; }
    echo "allowlist lines: $(grep -cE '^[[:space:]]*\(libc::SYS_.*//' src/seccomp.rs)"
    return $ok
}

# AC-7.2 (static part): run_boot sets PR_SET_NO_NEW_PRIVS before it spawns the first
# thread and before the first filter is installed.
no_new_privs_first() {
    local nnp first
    nnp=$(grep -n 'seccomp::set_no_new_privs()' src/lib.rs | head -1 | cut -d: -f1)
    first=$(grep -nE 'spawn_stdin_reader\(|vcpu::spawn\(|seccomp::install\(' src/lib.rs | head -1 | cut -d: -f1)
    echo "set_no_new_privs at line ${nnp:-none}; first spawn/install at line ${first:-none}"
    grep -q 'PR_SET_NO_NEW_PRIVS' src/seccomp.rs && [ -n "$nnp" ] && [ -n "$first" ] && [ "$nnp" -lt "$first" ]
}

# AC-7.3: the M4-M6 checks above ran with seccomp on (the default): all passed, and
# none of their tests disables seccomp.
m4_to_m6_with_seccomp() {
    local ok=0
    if grep -n -- '--no-seccomp' tests/m4_virtio_blk.rs tests/m5_virtio_net.rs tests/m6_smp.rs; then
        echo "an M4-M6 test disables seccomp"; ok=1
    fi
    for id in $failed_ids; do
        case $id in AC-4.*|AC-5.*|AC-6.*) echo "$id did not pass"; ok=1 ;; esac
    done
    return $ok
}

# AC-7.6: every syscall each thread type allows (the common list plus its own) is
# named in that thread's subsection of "## Seccomp allowlists" in notes/phase-2.md.
allowlists_in_notes() {
    local ok=0 kind heading sc
    for kind in Main Vcpu Stdin; do
        case $kind in
            Main) heading="### main thread" ;;
            Vcpu) heading="### vCPU threads" ;;
            Stdin) heading="### stdin thread" ;;
        esac
        section=$(awk -v h="$heading" '
            /^## / { in_allow = ($0 == "## Seccomp allowlists"); on = 0; next }
            in_allow && /^### / { on = ($0 == h); next }
            on { print }
        ' notes/phase-2.md)
        # Common entries come before `match kind`; the kind's own after `ThreadKind::<kind> =>`.
        for sc in $(awk -v k="ThreadKind::$kind =>" '
            /match kind/ { common = 0; done = 1 }
            /let mut list/ { common = 1 }
            index($0, k) { mine = 1; next }
            /ThreadKind::[A-Za-z]+ =>/ { mine = 0 }
            (common || mine) && match($0, /\(libc::SYS_[a-z0-9_]+/) { print substr($0, RSTART + 11, RLENGTH - 11) }
        ' src/seccomp.rs); do
            if grep -q "\`$sc\`" <<<"$section"; then
                echo "$heading: $sc"
            else
                echo "$heading: $sc MISSING"; ok=1
            fi
        done
    done
    return $ok
}

# AC-8.1: the guest-facing parsers live in queue.rs/parse.rs over the crate's own
# GuestMem trait, with no KVM (or vm-memory) dependency.
pure_parsers() {
    local ok=0 f
    grep -n 'pub trait GuestMem' src/devices/virtio/queue.rs || { echo "no GuestMem trait"; ok=1; }
    for f in src/devices/virtio/queue.rs src/devices/virtio/parse.rs; do
        if grep -nE 'kvm_ioctls|kvm_bindings|vm_memory' "$f"; then echo "$f depends on KVM"; ok=1; fi
    done
    for fn in 'pub fn walk_chain' 'pub fn pop' 'pub fn add_used' 'pub fn parse_blk_request' \
        'pub fn parse_net_tx' 'pub fn zero_vnet_header'; do
        grep -qE "$fn" src/devices/virtio/queue.rs src/devices/virtio/parse.rs \
            || { echo "missing: $fn"; ok=1; }
    done
    return $ok
}

KANI_HARNESSES="pop_stays_in_ring walk_chain_bounded blk_parse_no_panic blk_parse_ranges_in_memory
blk_parse_sector_bounds net_tx_bounds net_tx_header_zeroed"

# AC-8.2: every harness verifies, all of them within 20 minutes together.
kani_proofs() {
    local start out ok=0 h n
    start=$(date +%s)
    out=$(timeout 1200 cargo kani 2>&1); local rc=$?
    echo "$out" | grep -E 'Checking harness|VERIFICATION:-|Verification Time|Complete -'
    echo "cargo kani exit $rc after $(( $(date +%s) - start )) s"
    [ "$rc" -eq 0 ] || ok=1
    # Each harness by name: its exact "Checking harness" line must be present, and the
    # first VERIFICATION line after it must be SUCCESSFUL. (No `grep | grep -m1`
    # pipeline: under pipefail the early exit's SIGPIPE would fail it.)
    for h in $KANI_HARNESSES; do
        header="Checking harness devices::virtio::proofs::$h..."
        if ! grep -qxF "$header" <<<"$out"; then
            echo "$h: not run"; ok=1; continue
        fi
        result=$(awk -v hdr="$header" '$0 == hdr { on = 1; next } on && /^VERIFICATION:-/ { print; exit }' <<<"$out")
        if [ "$result" = "VERIFICATION:- SUCCESSFUL" ]; then
            echo "$h: SUCCESSFUL"
        else
            echo "$h: ${result:-no result}"; ok=1
        fi
    done
    n=$(grep -c 'VERIFICATION:- SUCCESSFUL' <<<"$out")
    [ "$n" -eq "$(wc -w <<<"$KANI_HARNESSES")" ] || { echo "$n harnesses successful"; ok=1; }
    return $ok
}

# AC-8.3: fuzz a temporary copy of the committed seed corpus (the fuzzer adds inputs
# to the corpus directory it runs on).
fuzz_blk() {
    local tmp rc before
    before=$(cd fuzz/corpus/blk_request && ls | xargs sha256sum)
    tmp=$(mktemp -d)
    cp fuzz/corpus/blk_request/* "$tmp"/
    cargo +nightly fuzz run blk_request "$tmp" -- -max_total_time="${FUZZ_SECONDS:-300}"
    rc=$?
    rm -rf "$tmp"
    [ "$before" = "$(cd fuzz/corpus/blk_request && ls | xargs sha256sum)" ] \
        || { echo "seed corpus changed"; return 1; }
    return $rc
}

# AC-8.4: notes list every regression test for Kani/fuzz findings (each must exist and
# pass), or say none were found.
regressions_listed() {
    local section tests t
    section=$(awk '/^## / { on = ($0 == "## Bugs found by Kani and fuzzing") ; next } on' notes/phase-2.md)
    [ -n "$section" ] || { echo "notes: no '## Bugs found by Kani and fuzzing' section"; return 1; }
    tests=$(grep -oE 'regression_[a-z0-9_]+' <<<"$section" | sort -u)
    if [ -z "$tests" ]; then
        grep -qi 'none were found' <<<"$section"
        return
    fi
    for t in $tests; do
        cargo test --lib -- "$t" 2>&1 | grep -q 'test result: ok. [1-9]' || { echo "$t missing or failing"; return 1; }
        echo "$t passes"
    done
}

# AC-P1: every acceptance criterion in the spec has a PASS/FAIL line in this script.
all_criteria_checked() {
    local ok=0 id
    for id in $(grep -oE '\*\*AC-[0-9P]+\.?[0-9]*\*\*' specs/phase-2/SPEC.md | tr -d '*' | sort -u); do
        [ "$id" = "AC-0.x" ] && continue
        [ "$id" = "AC-P1" ] && continue
        grep -qE "^(tap_)?check $id " "$0" || { echo "$id has no check"; ok=1; }
    done
    return $ok
}

# Words in the part of notes/phase-2.md under heading $1 (up to the next heading of
# the same or higher level). With $2, heading $1 only counts inside section $2.
notes_section_words() {
    awk -v h="$1" -v parent="${2:-}" '
        parent != "" && index($0, parent) == 1 { in_parent = 1; next }
        parent != "" && /^## / { in_parent = 0 }
        on && /^#+ / && index($0, " ") <= level { on = 0 }
        (parent == "" || in_parent) && index($0, h) == 1 { on = 1; level = index($0, " "); next }
        on { print }
    ' notes/phase-2.md | wc -w
}

# AC-P2: the notes explain each milestone, the request path, the seccomp allowlists,
# the Kani properties and their limits, and have >= 12 answered interview questions.
notes_complete() {
    local ok=0 words heading answered
    for heading in "## What each milestone does" "## How a virtio request travels" \
        "## Seccomp allowlists" "## Kani properties and their limits"; do
        words=$(notes_section_words "$heading")
        echo "$heading: $words words"
        [ "$words" -ge 50 ] || { echo "  -> too short or missing (need >= 50 words)"; ok=1; }
    done
    for heading in "### M4" "### M5" "### M6" "### M7" "### M8"; do
        words=$(notes_section_words "$heading" "## What each milestone does")
        echo "$heading explanation: $words words"
        [ "$words" -ge 50 ] || { echo "  -> too short or missing (need >= 50 words)"; ok=1; }
    done
    grep -qxF '### What the proofs do not cover' \
        <<<"$(awk '/^## / { on = ($0 == "## Kani properties and their limits"); next } on' notes/phase-2.md)" \
        || { echo "Kani section has no '### What the proofs do not cover' subsection"; ok=1; }
    words=$(notes_section_words "### What the proofs do not cover" "## Kani properties and their limits")
    echo "### What the proofs do not cover: $words words"
    [ "$words" -ge 50 ] || { echo "  -> too short (need >= 50 words)"; ok=1; }
    answered=$(awk '
        /^\*\*Q[0-9]+\./ { pending = 1; next }
        pending && NF { if ($0 !~ /^(\*\*Q[0-9]+\.|#)/) n++; pending = 0 }
        END { print n + 0 }
    ' notes/phase-2.md)
    echo "answered interview questions: $answered"
    [ "$answered" -ge 12 ] || { echo "  -> need at least 12"; ok=1; }
    return $ok
}

# Ignored tests boot VMs (and, from M5, share one TAP), so they never run concurrently.
itest() { cargo test --test "$1" -- --include-ignored --exact "$2" --test-threads=1; }

# Setup: build the VMM and every artifact (their failures surface in the checks below).
cargo build >"$LOG_DIR/build.log" 2>&1 || echo "note: cargo build failed (see $LOG_DIR/build.log)"
scripts/build-kernel.sh >"$LOG_DIR/build-kernel.log" 2>&1 || echo "note: build-kernel.sh failed (see $LOG_DIR/build-kernel.log)"
if [ ! -f artifacts/initramfs.cpio ]; then
    scripts/build-initramfs.sh >"$LOG_DIR/build-initramfs.log" 2>&1 || echo "note: build-initramfs.sh failed"
fi
scripts/build-disk.sh >"$LOG_DIR/build-disk.log" 2>&1 || echo "note: build-disk.sh failed (see $LOG_DIR/build-disk.log)"

echo "== Always =="
check AC-0.1 "cargo fmt --check" cargo fmt --check
check AC-0.2 "cargo clippy --all-targets -- -D warnings" cargo clippy --all-targets -- -D warnings
check AC-0.3 "every unsafe block has a SAFETY comment" safety_comments
check AC-0.4 "unopenable KVM device -> exit 2, path in error" missing_kvm_device
check AC-0.5 "scripts/verify-phase-1.sh passes on the Phase 2 kernel" phase1_regression

echo "== M4 — virtio-blk =="
m4() { itest m4_virtio_blk "$1"; }
check AC-4.1 "kernel config has the virtio-mmio/blk, ext4 and MP-table options" kernel_config
check AC-4.2 "build-disk.sh makes a 64 MiB ext4 image with /hello.txt, without root" disk_image
check AC-4.3 "/sys/block/vda/size is 131072" m4 ac_4_3_disk_capacity_is_64_mib
check AC-4.4 "mount /dev/vda; /mnt/hello.txt is phase2-disk" m4 ac_4_4_mount_and_read_hello
check AC-4.5 "writes persist across VMM processes (guest and debugfs)" m4 ac_4_5_writes_persist_across_vmm_processes
check AC-4.6 "32 MiB random file: guest and host sha256 match" m4 ac_4_6_32_mib_file_integrity
check AC-4.7 "--disk-ro: write fails read-only, image unchanged" m4 ac_4_7_read_only_disk
check AC-4.8 "bad --disk / --disk-ro arguments -> exit 2" m4 ac_4_8_bad_disk_arguments_exit_2
check AC-4.9 "unit tests: OUT on RO -> IOERR; GET_ID/unknown -> UNSUPP" \
    unit_tests 2 \
    devices::virtio::block::tests::out_on_read_only_device_is_ioerr_and_file_unchanged \
    devices::virtio::block::tests::get_id_and_unknown_types_are_unsupp

echo "== M5 — virtio-net =="
m5() { itest m5_virtio_net "$1"; }
check AC-5.1 "kernel config has VIRTIO_NET=y" kernel_config VIRTIO_NET
tap_check AC-5.2 "guest eth0 has MAC 06:00:ac:10:00:02" m5 ac_5_2_guest_sees_default_mac
tap_check AC-5.3 "guest pings 172.16.0.1 with 0% loss" m5 ac_5_3_guest_pings_host
tap_check AC-5.4 "host pings 172.16.0.2 with 0% loss" m5 ac_5_4_host_pings_guest
tap_check AC-5.5 "16 MiB HTTP download: guest and host sha256 match" m5 ac_5_5_bulk_download_matches_host_checksum
check AC-5.6 "bad --tap / --guest-mac arguments -> exit 2" m5 ac_5_6_bad_net_arguments_exit_2

echo "== M6 — Multiple vCPUs =="
m6() { itest m6_smp "$1"; }
check AC-6.1 "--vcpus 0 and --vcpus 5 -> exit 2" m6 ac_6_1_vcpu_count_out_of_range_exit_2
check AC-6.2 "--vcpus 4: nproc and /proc/cpuinfo show 4" m6 ac_6_2_guest_sees_four_cpus
check AC-6.3 "--vcpus 4: LOC interrupts on all 4 CPUs" m6 ac_6_3_every_cpu_takes_timer_interrupts
tap_check AC-6.4 "10 boots with 4 vCPUs + disk + TAP (each <= 30 s, exit 0)" m6 ac_6_4_ten_boots_with_disk_net_and_four_vcpus
check AC-6.5 "--vcpus 4: reboot -f exits 0 within 10 s" m6 ac_6_5_reboot_with_four_vcpus_exits_within_10s
tap_check AC-6.6 "all M4 and M5 tests pass with 4 vCPUs" m4_m5_with_vcpus 4

echo "== M7 — Hardening =="
m7() { itest m7_seccomp "$1"; }
# The runtime test checks both `Seccomp: 2` and `NoNewPrivs: 1` on every thread.
ac_7_1() { seccomp_source && m7 ac_7_1_every_thread_is_filtered; }
ac_7_2() { no_new_privs_first && m7 ac_7_1_every_thread_is_filtered; }
check AC-7.1 "filters only in seccomp.rs, one comment per syscall; every thread filtered" ac_7_1
check AC-7.2 "PR_SET_NO_NEW_PRIVS before the first thread/filter; NoNewPrivs on all threads" ac_7_2
check AC-7.3 "all M4, M5 and M6 criteria pass with seccomp enabled" m4_to_m6_with_seccomp
check AC-7.4 "forbidden syscall on a vCPU kills the VMM with SIGSYS" m7 ac_7_4_forbidden_syscall_kills_vmm_with_sigsys
check AC-7.5 "--no-seccomp warns on stderr and still boots" m7 ac_7_5_no_seccomp_warns_and_boots
check AC-7.6 "notes/phase-2.md lists every allowed syscall per thread type" allowlists_in_notes

echo "== M8 — Verification =="
check AC-8.1 "queue/parse are pure functions over GuestMem, no KVM" pure_parsers
check AC-8.2 "cargo kani: all 7 harnesses SUCCESSFUL within 20 min" kani_proofs
check AC-8.3 "fuzz blk_request for ${FUZZ_SECONDS:-300} s on a corpus copy: no crashes" fuzz_blk
check AC-8.4 "every Kani/fuzz finding has a regression test listed in the notes" regressions_listed

echo "== Phase 2 done =="
check AC-P1 "every spec criterion has a PASS/FAIL line here" all_criteria_checked
check AC-P2 "notes/phase-2.md: milestones, request path, allowlists, Kani, >= 12 questions" notes_complete

if [ "$failed" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$failed"
