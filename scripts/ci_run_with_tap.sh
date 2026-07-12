#!/usr/bin/env bash
# Create a TAP in the current network namespace and run kitsune with --tap.
# Intended to be invoked under: unshare --user --net --map-root-user
set -euo pipefail

TAP="${KITSUNE_TAP_NAME:-kitsune-e2e0}"
HOST_IP="${KITSUNE_TAP_HOST_IP:-192.168.77.1/24}"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <kitsune-bin> [kitsune args...]" >&2
  exit 2
fi
BIN="$1"
shift

ip tuntap add mode tap name "$TAP"
ip addr add "$HOST_IP" dev "$TAP"
ip link set "$TAP" up

exec "$BIN" "$@" --tap "$TAP"
