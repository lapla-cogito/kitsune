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
  ro=0
  if [ -r /sys/block/vda/ro ]; then
    ro=$(cat /sys/block/vda/ro)
  fi
  mkdir -p /mnt
  if [ "$ro" = 1 ]; then
    # Read-only virtio-blk: mount (if possible) and confirm writes fail.
    if mount -t ext4 -o ro /dev/vda /mnt 2>/dev/null \
      || mount -o ro /dev/vda /mnt 2>/dev/null; then
      if echo no > /mnt/kitsune-should-fail 2>/dev/null; then
        echo "kitsune-blk-ro-fail"
      else
        echo "kitsune-blk-ro-ok"
      fi
      umount /mnt 2>/dev/null || true
    else
      # Still RO at the block layer even if mount is unavailable.
      echo "kitsune-blk-ro-ok"
    fi
  else
    # Read-write: small write/readback, then multi-block (~32 KiB) I/O.
    io_ok=0
    bulk_ok=0
    if mount -t ext4 /dev/vda /mnt 2>/dev/null || mount /dev/vda /mnt 2>/dev/null; then
      if echo kitsune-data > /mnt/t && sync; then
        umount /mnt 2>/dev/null || true
        if mount -t ext4 /dev/vda /mnt 2>/dev/null || mount /dev/vda /mnt 2>/dev/null; then
          if grep -q kitsune-data /mnt/t 2>/dev/null; then
            io_ok=1
          fi
        fi
      fi
      if [ "$io_ok" = 1 ]; then
        if dd if=/dev/zero of=/mnt/bulk bs=1024 count=32 conv=fsync 2>/dev/null \
          || dd if=/dev/zero of=/mnt/bulk bs=1024 count=32 2>/dev/null; then
          sync
          umount /mnt 2>/dev/null || true
          if mount -t ext4 /dev/vda /mnt 2>/dev/null || mount /dev/vda /mnt 2>/dev/null; then
            sz=$(wc -c < /mnt/bulk 2>/dev/null || echo 0)
            sz=$(echo "$sz" | tr -d ' \n')
            if [ -n "$sz" ] && [ "$sz" -ge 32768 ] 2>/dev/null; then
              bulk_ok=1
            fi
          fi
        fi
      fi
      umount /mnt 2>/dev/null || true
    fi
    if [ "$io_ok" = 1 ]; then
      echo "kitsune-blk-io-ok"
    else
      echo "kitsune-blk-io-fail"
    fi
    if [ "$bulk_ok" = 1 ]; then
      echo "kitsune-blk-bulk-ok"
    else
      echo "kitsune-blk-bulk-fail"
    fi
  fi
fi
# virtio-net: offload features, static addressing, then host reachability tests.
if [ -d /sys/class/net/eth0 ]; then
  # Negotiated virtio features: 64 chars of 0/1, index = feature bit.
  feats=""
  if [ -r /sys/class/net/eth0/device/features ]; then
    feats=$(cat /sys/class/net/eth0/device/features | tr -d '\n')
  else
    for d in /sys/bus/virtio/devices/virtio*; do
      [ -r "$d/device" ] || continue
      id=$(cat "$d/device" 2>/dev/null)
      case "$id" in
        1|0x1|0x01|0x0001|0x00000001)
          feats=$(cat "$d/features" 2>/dev/null | tr -d '\n')
          break
          ;;
      esac
    done
  fi
  echo "kitsune-net-features=$feats"
  # Bits: CSUM=0 GUEST_CSUM=1 GUEST_TSO4=7 GUEST_TSO6=8 HOST_TSO4=11 HOST_TSO6=12
  bit() {
    echo "$feats" | cut -c"$(($1 + 1))"
  }
  if [ "$(bit 0)" = 1 ] && [ "$(bit 1)" = 1 ] \
    && [ "$(bit 7)" = 1 ] && [ "$(bit 8)" = 1 ] \
    && [ "$(bit 11)" = 1 ] && [ "$(bit 12)" = 1 ]; then
    echo "kitsune-net-offload-ok"
  else
    echo "kitsune-net-offload-fail"
  fi

  ip link set eth0 up
  ip addr add 192.168.77.2/24 dev eth0

  # ICMP: basic reachability.
  if ping -c 3 -W 1 192.168.77.1 >/dev/null 2>&1 \
    || ping -c 3 192.168.77.1 >/dev/null 2>&1; then
    echo "kitsune-net-ok"
  else
    echo "kitsune-net-fail"
  fi

  tcp_ok=0
  for _ in 1 2 3; do
    resp=$(printf 'x\n' | nc -w 1 192.168.77.1 7777 2>/dev/null) \
      || resp=$(printf 'x\n' | nc 192.168.77.1 7777 2>/dev/null) \
      || resp=""
    case "$resp" in
      *kitsune-host-tcp-ok*)
        tcp_ok=1
        break
        ;;
    esac
    sleep 0.2 2>/dev/null || sleep 1
  done
  if [ "$tcp_ok" = 1 ]; then
    echo "kitsune-net-tcp-ok"
  else
    echo "kitsune-net-tcp-fail"
  fi

  if ping -c 20 -W 1 192.168.77.1 >/dev/null 2>&1 \
    || ping -c 20 192.168.77.1 >/dev/null 2>&1; then
    echo "kitsune-net-bulk-ok"
  else
    echo "kitsune-net-bulk-fail"
  fi
fi
while true; do sleep 3600; done
