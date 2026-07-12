#!/usr/bin/env bash
# Prepare guest artifacts for CI/integration tests (vmlinux + busybox initrd).
set -euo pipefail

OUT="${1:-target/ci-guest}"
mkdir -p "$OUT"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GUEST_INIT="${SCRIPT_DIR}/guest_init.sh"
if [[ ! -f "$GUEST_INIT" ]]; then
  echo "error: missing $GUEST_INIT" >&2
  exit 1
fi

VMLINUX_URL="${KITSUNE_VMLINUX_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260708-e8a198e23f48-0/x86_64/vmlinux-6.18.36}"

content_hash() {
  sha256sum | cut -c1-16
}

VMLINUX_HASH="$(printf '%s' "$VMLINUX_URL" | content_hash)"
if [[ ! -f "$OUT/vmlinux" || ! -f "$OUT/.vmlinux-$VMLINUX_HASH" ]]; then
  echo "downloading vmlinux..."
  wget -q -O "$OUT/vmlinux" "$VMLINUX_URL"
  rm -f "$OUT"/.vmlinux-*
  touch "$OUT/.vmlinux-$VMLINUX_HASH"
fi

BB="$(command -v busybox)"
# Inputs that affect the packed initrd image.
INITRD_HASH="$(
  {
    cat "$GUEST_INIT"
    # Host busybox identity (path + banner); package updates force a rebuild.
    printf 'busybox=%s\n' "$BB"
    "$BB" 2>&1 | head -n1 || true
  } | content_hash
)"

if [[ ! -f "$OUT/initrd.img" || ! -f "$OUT/.initrd-$INITRD_HASH" ]]; then
  echo "building busybox initrd (hash $INITRD_HASH)..."
  WORK="$(mktemp -d)"
  mkdir -p "$WORK"/{bin,dev,proc,sys}
  cp "$BB" "$WORK/bin/busybox"
  ln -s busybox "$WORK/bin/sh"
  cp "$GUEST_INIT" "$WORK/init"
  chmod +x "$WORK/init"
  ( cd "$WORK" && find . | cpio -o -H newc ) | gzip -9 > "$OUT/initrd.img"
  rm -rf "$WORK"
  rm -f "$OUT"/.initrd-*
  touch "$OUT/.initrd-$INITRD_HASH"
fi

if [[ ! -f "$OUT/disk.ext4" ]]; then
  echo "building test disk image..."
  dd if=/dev/zero of="$OUT/disk.ext4" bs=1M count=8 status=none
  if command -v mkfs.ext4 >/dev/null 2>&1; then
    mkfs.ext4 -F -q "$OUT/disk.ext4"
  fi
fi

echo "guest artifacts ready in $OUT"
ls -lh "$OUT"
