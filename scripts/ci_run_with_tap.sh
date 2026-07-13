#!/usr/bin/env bash
# Create a TAP in the current network namespace and run kitsune with --tap.
# Intended to be invoked under: unshare --user --net --map-root-user
set -euo pipefail

TAP="${KITSUNE_TAP_NAME:-kitsune-e2e0}"
HOST_IP="${KITSUNE_TAP_HOST_IP:-192.168.77.1/24}"
TCP_PORT="${KITSUNE_E2E_TCP_PORT:-7777}"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <kitsune-bin> [kitsune args...]" >&2
  exit 2
fi
BIN="$1"
shift

ip tuntap add mode tap name "$TAP"
ip addr add "$HOST_IP" dev "$TAP"
ip link set "$TAP" up

# Guest e2e TCP client hits this; keep accepting until kitsune exits.
HOST_ADDR="${HOST_IP%%/*}"
python3 - "$HOST_ADDR" "$TCP_PORT" <<'PY' &
import socket, sys
host, port = sys.argv[1], int(sys.argv[2])
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((host, port))
s.listen(8)
s.settimeout(1.0)
while True:
    try:
        c, _ = s.accept()
    except socket.timeout:
        continue
    try:
        c.sendall(b"kitsune-host-tcp-ok\n")
    finally:
        c.close()
PY
TCP_PID=$!
cleanup() {
  kill "$TCP_PID" 2>/dev/null || true
}
trap cleanup EXIT

exec "$BIN" "$@" --tap "$TAP"
