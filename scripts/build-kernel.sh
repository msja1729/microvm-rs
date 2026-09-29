#!/usr/bin/env bash
# Download the pinned Linux 6.12 LTS release, verify it, configure it with
# x86_64_defconfig + kvm_guest.config + configs/phase2.config, and build
# artifacts/bzImage (plus artifacts/kernel.config, the .config it was built from).
# Idempotent: does nothing if artifacts/bzImage was already built from this version
# and this exact config fragment (the stamp records both).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=6.12.111
SHA256=9e59dc67624188fa12a6601f9598499cd6662a9066be572b59f935e3d7849810
URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${VERSION}.tar.xz"

BUILD_DIR=artifacts/build
TARBALL="$BUILD_DIR/linux-${VERSION}.tar.xz"
SRC="$BUILD_DIR/linux-${VERSION}"
STAMP=artifacts/bzImage.version
FRAGMENT=configs/phase2.config

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

FRAGMENT_SHA=$(sha256sum "$FRAGMENT" | cut -d' ' -f1)
WANT_STAMP="$VERSION $FRAGMENT_SHA"
if [ -f artifacts/bzImage ] && [ -f artifacts/kernel.config ] \
    && [ "$(cat "$STAMP" 2>/dev/null)" = "$WANT_STAMP" ]; then
    echo "artifacts/bzImage is already Linux $VERSION with $FRAGMENT ($FRAGMENT_SHA)"
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
# -m: merge the fragment into .config only; olddefconfig then resolves dependencies.
(cd "$SRC" && ./scripts/kconfig/merge_config.sh -m .config "$OLDPWD/$FRAGMENT")
make -C "$SRC" olddefconfig
# A fragment line Kconfig dropped (e.g. an unmet dependency) would silently build
# a kernel without the feature; fail instead.
while IFS= read -r line; do
    [ -z "$line" ] && continue
    grep -qxF "$line" "$SRC/.config" || { echo "$FRAGMENT: '$line' missing from .config" >&2; exit 1; }
done < "$FRAGMENT"
make -C "$SRC" -j"$(nproc)" bzImage

cp "$SRC/arch/x86/boot/bzImage" artifacts/bzImage
cp "$SRC/.config" artifacts/kernel.config
echo "$WANT_STAMP" > "$STAMP"
echo "built artifacts/bzImage (Linux $VERSION, $FRAGMENT $FRAGMENT_SHA)"
