#!/usr/bin/env bash
# Download the pinned Linux 6.12 LTS release, verify it, configure it with
# x86_64_defconfig + kvm_guest.config, and build artifacts/bzImage.
# Idempotent: does nothing if artifacts/bzImage was already built from this version.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=6.12.111
SHA256=9e59dc67624188fa12a6601f9598499cd6662a9066be572b59f935e3d7849810
URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${VERSION}.tar.xz"

BUILD_DIR=artifacts/build
TARBALL="$BUILD_DIR/linux-${VERSION}.tar.xz"
SRC="$BUILD_DIR/linux-${VERSION}"
STAMP=artifacts/bzImage.version

missing=()
for cmd in curl sha256sum tar xz make gcc flex bison bc; do
    command -v "$cmd" >/dev/null || missing+=("$cmd")
done
for header in /usr/include/libelf.h /usr/include/openssl/ssl.h; do
    [ -e "$header" ] || missing+=("$header")
done
if [ "${#missing[@]}" -gt 0 ]; then
    echo "missing: ${missing[*]}" >&2
    echo "install with: sudo apt install build-essential flex bison bc libelf-dev libssl-dev xz-utils curl" >&2
    exit 1
fi

if [ -f artifacts/bzImage ] && [ "$(cat "$STAMP" 2>/dev/null)" = "$VERSION" ]; then
    echo "artifacts/bzImage is already Linux $VERSION"
    exit 0
fi

mkdir -p "$BUILD_DIR"
if [ ! -f "$TARBALL" ]; then
    curl -fL --retry 3 -o "$TARBALL.part" "$URL"
    mv "$TARBALL.part" "$TARBALL"
fi
echo "$SHA256  $TARBALL" | sha256sum -c -

if [ ! -d "$SRC" ]; then
    tar -xf "$TARBALL" -C "$BUILD_DIR"
fi

make -C "$SRC" x86_64_defconfig
make -C "$SRC" kvm_guest.config
make -C "$SRC" -j"$(nproc)" bzImage

cp "$SRC/arch/x86/boot/bzImage" artifacts/bzImage
echo "$VERSION" > "$STAMP"
echo "built artifacts/bzImage (Linux $VERSION)"
