#!/usr/bin/env bash
# Build a self-contained release bundle in dist/:
#   dist/microvm-rs-<version>-x86_64-linux.tar.gz   (vmm binary, kernel, initramfs, disk, guests)
#   dist/SHA256SUMS
# Uses the existing artifacts/ (run the build-*.sh scripts first). Never uses sudo.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=${1:-v$(cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')}
NAME=microvm-rs-$VERSION-x86_64-linux
OUT=dist/$NAME

for f in artifacts/bzImage artifacts/initramfs.cpio artifacts/disk.ext4 artifacts/kernel.config; do
    [ -f "$f" ] || { echo "missing $f; run the scripts/build-*.sh scripts first" >&2; exit 1; }
done

cargo build --release
scripts/build-guests.sh

rm -rf "$OUT" "dist/$NAME.tar.gz"
mkdir -p "$OUT"
install -m 755 target/release/vmm "$OUT/vmm"
strip "$OUT/vmm"
cp artifacts/bzImage artifacts/initramfs.cpio artifacts/disk.ext4 "$OUT/"
cp artifacts/kernel.config "$OUT/kernel.config"
cp guests/build/hello16.bin guests/build/hello64.bin "$OUT/"
cp README.md LICENSE "$OUT/"

cat >"$OUT/run.sh" <<'EOF'
#!/usr/bin/env bash
# Boot Linux in a microVM with a virtio disk and $VCPUS vCPUs (default 2, max 4).
# Type `reboot -f` in the guest to exit. Extra arguments are passed to `vmm boot`
# (e.g. --tap vmtap0, --disk-ro, --mem-mib 1024).
set -euo pipefail
cd "$(dirname "$0")"
if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    echo "/dev/kvm is not accessible; add yourself to the kvm group: sudo usermod -aG kvm \$USER" >&2
    exit 1
fi
exec ./vmm boot --kernel bzImage --initrd initramfs.cpio --disk disk.ext4 --vcpus "${VCPUS:-2}" "$@"
EOF
chmod 755 "$OUT/run.sh"

tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
(cd dist && sha256sum "$NAME.tar.gz" >SHA256SUMS)
cat dist/SHA256SUMS
