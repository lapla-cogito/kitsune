#!/usr/bin/env bash
# Prepare guest artifacts for CI/integration tests (vmlinux + busybox initrd).
set -euo pipefail

OUT="${1:-target/ci-guest}"
mkdir -p "$OUT"

VMLINUX_URL="${KITSUNE_VMLINUX_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260708-e8a198e23f48-0/x86_64/vmlinux-6.18.36}"
# Bump when init contents change so CI/local caches rebuild.
INITRD_STAMP="v3-smp"

if [[ ! -f "$OUT/vmlinux" ]]; then
  echo "downloading vmlinux..."
  wget -q -O "$OUT/vmlinux" "$VMLINUX_URL"
fi

if [[ ! -f "$OUT/initrd.img" || ! -f "$OUT/.initrd-$INITRD_STAMP" ]]; then
  echo "building busybox initrd ($INITRD_STAMP)..."
  BB="$(command -v busybox)"
  WORK="$(mktemp -d)"
  mkdir -p "$WORK"/{bin,dev,proc,sys}
  cp "$BB" "$WORK/bin/busybox"
  ln -s busybox "$WORK/bin/sh"
  cat > "$WORK/init" << 'INIT'
#!/bin/sh
/bin/busybox --install -s /bin
export PATH=/bin:/sbin HOME=/
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev 2>/dev/null || true
echo "kitsune-initrd-ok"
# Online CPU count (SMP / MADT bring-up).
cpus=$(nproc 2>/dev/null || grep -c '^processor' /proc/cpuinfo)
echo "kitsune-cpus=$cpus"
if [ "$cpus" -ge 2 ]; then
  echo "kitsune-smp-ok"
fi
if [ -e /dev/vda ]; then
  echo "kitsune-blk-ok"
fi
# virtio-net: static addressing then ping the host TAP side.
if [ -d /sys/class/net/eth0 ]; then
  ip link set eth0 up
  ip addr add 192.168.77.2/24 dev eth0
  # BusyBox ping: -c count, -W seconds per reply (if supported).
  if ping -c 3 -W 2 192.168.77.1 >/dev/null 2>&1 \
    || ping -c 3 192.168.77.1 >/dev/null 2>&1; then
    echo "kitsune-net-ok"
  else
    echo "kitsune-net-fail"
  fi
fi
while true; do sleep 3600; done
INIT
  chmod +x "$WORK/init"
  ( cd "$WORK" && find . | cpio -o -H newc ) | gzip -9 > "$OUT/initrd.img"
  rm -rf "$WORK"
  rm -f "$OUT"/.initrd-*
  touch "$OUT/.initrd-$INITRD_STAMP"
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
