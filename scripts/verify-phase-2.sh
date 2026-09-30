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
    fi
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

if [ "$failed" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$failed"
