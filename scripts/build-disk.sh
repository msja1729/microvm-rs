#!/usr/bin/env bash
# Build artifacts/disk.ext4: a 64 MiB ext4 image containing /hello.txt ("phase2-disk"),
# without root (mkfs.ext4 -d copies a staging directory into the new filesystem).
# Rebuilds on every run; it takes well under a second.
set -euo pipefail
cd "$(dirname "$0")/.."

missing=()
for cmd in mkfs.ext4 debugfs truncate; do
    command -v "$cmd" >/dev/null || missing+=("$cmd")
done
if [ "${#missing[@]}" -gt 0 ]; then
    echo "missing: ${missing[*]}" >&2
    echo "install with: sudo apt install e2fsprogs" >&2
    exit 1
fi

OUT=artifacts/disk.ext4
mkdir -p artifacts
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

echo phase2-disk > "$STAGE/hello.txt"
rm -f "$OUT"
truncate -s 64M "$OUT"
mkfs.ext4 -q -F -d "$STAGE" "$OUT"
echo "built $OUT"
