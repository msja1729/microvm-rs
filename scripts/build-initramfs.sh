#!/usr/bin/env bash
# Pack static BusyBox and the Phase 1 /init script into artifacts/initramfs.cpio.
# Idempotent: rebuilds the archive from scratch every time (it takes well under a second).
set -euo pipefail
cd "$(dirname "$0")/.."

BUSYBOX=/bin/busybox
missing=()
command -v cpio >/dev/null || missing+=(cpio)
if ! file -L "$BUSYBOX" 2>/dev/null | grep -q 'statically linked'; then
    missing+=("static $BUSYBOX")
fi
if [ "${#missing[@]}" -gt 0 ]; then
    echo "missing: ${missing[*]}" >&2
    echo "install with: sudo apt install busybox-static cpio" >&2
    exit 1
fi
for applet in sh mount setsid cttyhack reboot uname; do
    if ! "$BUSYBOX" --list | grep -qx "$applet"; then
        echo "$BUSYBOX has no '$applet' applet" >&2
        exit 1
    fi
done

STAGE=artifacts/build/initramfs
rm -rf "$STAGE"
mkdir -p "$STAGE"/{bin,dev,proc,sys}
cp "$BUSYBOX" "$STAGE/bin/busybox"
for applet in $("$BUSYBOX" --list); do
    [ "$applet" = busybox ] || ln -s busybox "$STAGE/bin/$applet"
done

# The archive contains no /dev/console node (creating one needs root), so the kernel
# starts /init with no stdin/stdout/stderr. /init mounts devtmpfs first and then
# attaches itself to /dev/console before printing anything.
cat > "$STAGE/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec 0</dev/console 1>/dev/console 2>&1
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
echo PHASE1-INIT-OK
export PS1='phase1# '
# cttyhack opens the real console tty (ttyS0) so the shell gets a controlling terminal.
exec setsid cttyhack /bin/sh
INIT
chmod 755 "$STAGE/init"

mkdir -p artifacts
(cd "$STAGE" && find . -mindepth 1 | LC_ALL=C sort | cpio --quiet -o -H newc -R 0:0) \
    > artifacts/initramfs.cpio
echo "built artifacts/initramfs.cpio"
