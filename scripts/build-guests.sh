#!/usr/bin/env bash
# Assemble every guests/*.asm into a flat binary under guests/build/.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v nasm >/dev/null; then
    echo "nasm not found; install it with: sudo apt install nasm" >&2
    exit 1
fi

mkdir -p guests/build
for src in guests/*.asm; do
    out="guests/build/$(basename "${src%.asm}").bin"
    nasm -f bin -o "$out" "$src"
    echo "built $out"
done
