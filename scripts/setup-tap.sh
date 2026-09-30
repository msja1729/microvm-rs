#!/usr/bin/env bash
# One-time host network setup for virtio-net tests, run by the user with sudo:
#   sudo scripts/setup-tap.sh
# Creates TAP interface vmtap0 owned by the invoking user (so the VMM can attach to it
# without privileges), gives the host 172.16.0.1/24 on it and brings it up.
# Idempotent. The TAP lasts until the next `wsl --shutdown` (or host reboot).
set -euo pipefail

TAP=vmtap0
HOST_ADDR=172.16.0.1/24

if [ "$(id -u)" -ne 0 ]; then
    echo "run as root: sudo $0" >&2
    exit 1
fi
if [ -z "${SUDO_USER:-}" ] || [ "$SUDO_USER" = root ]; then
    echo "SUDO_USER is not set: run this with sudo from your normal user account" >&2
    exit 1
fi
if [ ! -c /dev/net/tun ]; then
    echo "/dev/net/tun is missing; this kernel has no TUN/TAP support" >&2
    exit 1
fi
command -v ip >/dev/null || { echo "missing ip; install with: sudo apt install iproute2" >&2; exit 1; }

uid=$(id -u "$SUDO_USER")
if [ -e "/sys/class/net/$TAP" ]; then
    owner=$(cat "/sys/class/net/$TAP/owner" 2>/dev/null || echo "?")
    if [ "$owner" != "$uid" ]; then
        echo "$TAP exists but is owned by uid $owner, not $SUDO_USER ($uid);" \
            "delete it with: sudo ip link delete $TAP" >&2
        exit 1
    fi
else
    ip tuntap add dev "$TAP" mode tap user "$SUDO_USER"
fi
ip addr replace "$HOST_ADDR" dev "$TAP"
ip link set "$TAP" up
echo "$TAP ready for $SUDO_USER with host address $HOST_ADDR"
echo "note: it lasts until the next 'wsl --shutdown'; run this script again after that"
