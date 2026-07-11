# kitsune

A KVM-based VMM written in Rust. Kitsune means "Fox" in Japanese.

## Features

- Direct boot of Linux `vmlinux` (ELF) or `bzImage`
- Serial console (COM1 <-> host stdin/stdout)
- Minimal ACPI (MADT / IOAPIC, COM1, virtio-mmio devices)
- Multi-vCPU (`--cpus`)
- virtio-blk (raw disk as `/dev/vda`)
- virtio-net (host TAP backend)

## Requirements

- Linux x86_64 with `/dev/kvm` (read/write)
- Rust toolchain
- For networking: `/dev/net/tun` and permission to create a TAP (usually root / `CAP_NET_ADMIN`)

```bash
# KVM access (re-login may be required after usermod)
sudo usermod -aG kvm "$USER"
# or run kitsune with sudo
```

## Build

```bash
cargo build --release
```

## Guest images

### Recommended: Firecracker CI kernel (Linux 6.18)

Prebuilt microVM kernel from [Firecracker CI artifacts](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md). Walks CI prefixes newest-first until a `vmlinux-6.18.*` is found.

```bash
#!/usr/bin/env bash
set -euo pipefail

arch=$(uname -m)
s3=https://s3.amazonaws.com/spec.ccfc.min
key=

while read -r prefix; do
  key=$(
    curl -fsSL "$s3?list-type=2&prefix=${prefix}${arch}/vmlinux-6.18." \
      | grep -oP '(?<=<Key>)[^<]+(?=</Key>)' \
      | grep -E '/vmlinux-6\.18\.[0-9]+$' \
      | sort -V | tail -1
  ) || true
  [[ -n $key ]] && break
done < <(
  curl -fsSL "$s3?list-type=2&prefix=firecracker-ci/&delimiter=/" \
    | grep -oP '(?<=<Prefix>)firecracker-ci/[0-9]{8}-[^/]+/(?=</Prefix>)' \
    | sort -r
)

[[ -n $key ]] || { echo "no vmlinux-6.18 found" >&2; exit 1; }
echo "Using $s3/$key"
wget -O vmlinux "$s3/$key"
```

Includes serial + virtio-mmio / virtio-blk / virtio-net.

### Busybox initrd

Minimal interactive shell initrd (gzipped cpio with `/init`):

```bash
# needs: busybox (static preferred), cpio, gzip
BB=$(command -v busybox)
WORKDIR=$(mktemp -d)
mkdir -p "$WORKDIR"/{bin,dev,proc,sys}
cp "$BB" "$WORKDIR/bin/busybox"
ln -s busybox "$WORKDIR/bin/sh"
cat > "$WORKDIR/init" << 'INIT'
#!/bin/sh
/bin/busybox --install -s /bin
export PATH=/bin:/sbin HOME=/ TERM=linux
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev 2>/dev/null || true
echo "initrd-ok"
exec /bin/busybox setsid /bin/busybox cttyhack /bin/busybox sh -c '
export PATH=/bin:/sbin HOME=/ PS1="# "
while printf "%s" "$PS1"
do
  IFS= read -r line || exit 0
  eval "$line"
done
'
INIT
chmod +x "$WORKDIR/init"
( cd "$WORKDIR" && find . | cpio -o -H newc ) | gzip -9 > initrd.img
rm -rf "$WORKDIR"
```

### Optional: Linux 7.x from source

Heavier (full kernel build). Useful for testing mainline guests newer than Firecracker’s prebuilts. Start from Firecracker’s 6.18 microVM config, then enable a few options kitsune needs:

```bash
VERSION=7.1.3
wget "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-${VERSION}.tar.xz"
tar xf "linux-${VERSION}.tar.xz"
cd "linux-${VERSION}"

wget -O .config \
  https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-x86_64-6.18.config

# Kitsune / userspace essentials (Firecracker base often omits some of these)
scripts/config --enable HYPERVISOR_GUEST
scripts/config --enable PARAVIRT
scripts/config --enable PARAVIRT_CLOCK
scripts/config --enable KVM_GUEST
scripts/config --enable BLK_DEV_INITRD
scripts/config --enable RD_GZIP
scripts/config --enable DEVTMPFS
scripts/config --enable DEVTMPFS_MOUNT
scripts/config --enable SERIAL_8250
scripts/config --enable SERIAL_8250_CONSOLE
scripts/config --enable VIRTIO
scripts/config --enable VIRTIO_MMIO
scripts/config --enable VIRTIO_MMIO_CMDLINE_DEVICES
scripts/config --enable VIRTIO_BLK
scripts/config --enable VIRTIO_NET
# PCI must be on so ACPICA builds PCI config space handlers (ACPI_PCI_CONFIGURED). Runtime still uses pci=off on the cmdline.
scripts/config --enable PCI
scripts/config --disable DEBUG_INFO_BTF   # if pahole is missing

make olddefconfig
make vmlinux -j"$(nproc)"
cp vmlinux ../vmlinux-7.1.3
```

### Optional: raw rootfs for virtio-blk

Any raw filesystem image the guest can mount (like ext4) works with `--block`. Example sketch:

```bash
dd if=/dev/zero of=rootfs.ext4 bs=1M count=256
mkfs.ext4 rootfs.ext4
# mount, install busybox/userspace, umount
```

## Usage

```
kitsune run --kernel <vmlinux|bzImage> [options]
kitsune run --flat-binary <path> [--load-addr N] [--entry N]
```

| Option | Description | Default |
|---|---|---|
| `--kernel` | Linux kernel (ELF vmlinux or bzImage) | - |
| `--initrd` | Initial ramdisk | - |
| `--block` | Raw disk as virtio-blk (`/dev/vda`) | - |
| `--tap` | Host TAP interface for virtio-net (e.g. `tap0`) | - |
| `--cmdline` | Kernel command line | `console=ttyS0 reboot=k panic=1 pci=off nomodule` |
| `--memory` | Guest RAM in MiB (kernel boot needs >= 32) | `256` |
| `--cpus` | Number of guest vCPUs (1–32; flat binary requires 1) | `1` |
| `--flat-binary` | Real-mode flat binary (mutually exclusive with `--kernel`) | - |
| `--load-addr` / `--entry` | Flat-binary GPA / entry (`CS.base = 0`) | `0` |

When `--initrd` is set and the cmdline has no `rdinit=`, kitsune appends `rdinit=/init`.  
Kernel boots always get minimal ACPI (MADT/IOAPIC + COM1).  
With `--block` / `--tap`, matching virtio-mmio devices are advertised in ACPI (and `virtio_mmio.device=` tokens are added for older kernels).  
With `--block`, `root=/dev/vda rw` is appended if no `root=` is present.

Guest COM1 (`0x3f8`) is wired to the host stdin/stdout. Do not redirect stdio if you need an interactive shell.

Paths below are examples (`vmlinux`, `initrd.img`, …) after you prepare images as in [Guest images](#guest-images).

### Initrd shell

```bash
./target/release/kitsune run \
  --kernel ./vmlinux \
  --initrd ./initrd.img \
  --memory 512
```

### Root filesystem on virtio-blk

```bash
./target/release/kitsune run \
  --kernel ./vmlinux \
  --block ./rootfs.ext4 \
  --memory 1024 \
  --cmdline "console=ttyS0 reboot=k panic=1 pci=off nomodule init=/bin/sh"
```

### Networking (virtio-net + TAP)

`--tap` only attaches the guest NIC to a host TAP. No IP, DHCP, or NAT.

1. Host: create TAP and enable NAT (replace `eth0` with your uplink interface):

```bash
sudo ip tuntap add mode tap name tap0
sudo ip addr add 192.168.100.1/24 dev tap0
sudo ip link set tap0 up

sudo sysctl -w net.ipv4.ip_forward=1
sudo iptables -t nat -A POSTROUTING -s 192.168.100.0/24 -o eth0 -j MASQUERADE
sudo iptables -A FORWARD -i tap0 -o eth0 -j ACCEPT
sudo iptables -A FORWARD -i eth0 -o tap0 -m state --state RELATED,ESTABLISHED -j ACCEPT
```

2. Start the guest (vmlinux + rootfs + TAP):

```bash
sudo ./target/release/kitsune run \
  --kernel ./vmlinux \
  --block ./rootfs.ext4 \
  --memory 1024 \
  --tap tap0 \
  --cmdline "console=ttyS0 reboot=k panic=1 pci=off nomodule init=/bin/sh"
```

3. Guest: bring up `eth0`, default route, DNS, then test:

```sh
ip link set eth0 up
ip addr add 192.168.100.2/24 dev eth0
ip route add default via 192.168.100.1
echo 'nameserver 8.8.8.8' > /etc/resolv.conf

curl -I example.com
```

## License

MIT
