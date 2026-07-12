//! Virtio-mmio network device (virtio 1.0 modern) backed by a host TAP.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _};
use virtio_queue::QueueT as _;

/// MMIO base for the virtio-net device (after virtio-blk window).
pub const VIRTIO_NET_MMIO_BASE: u64 = 0xd000_1000;
/// GSI / IRQ line for virtio-net.
pub const VIRTIO_NET_IRQ: u32 = 6;

const VIRTIO_ID_NET: u32 = 1;

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const DEVICE_FEATURES: u64 =
    super::virtio_mmio::VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS;

const VIRTIO_NET_S_LINK_UP: u16 = 1;

const QUEUE_RX: u32 = 0;
const QUEUE_TX: u32 = 1;
const QUEUE_MAX_SIZE: u16 = 256;

/// Linux uses `sizeof(virtio_net_hdr_mrg_rxbuf)` (= 12) whenever
/// `VIRTIO_F_VERSION_1` is negotiated, even without `MRG_RXBUF`.
const NET_HDR_LEN: usize = 12;
/// Ethernet frame + virtio_net_hdr (no VLAN jumbo).
const MAX_FRAME: usize = 1514 + NET_HDR_LEN;

const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;
const IFF_VNET_HDR: libc::c_short = 0x4000;

const CONFIG_SIZE: usize = 8; // mac[6] + status[2]

/// Virtio-mmio network device using a host TAP interface.
pub struct VirtioNet {
    mmio: super::virtio_mmio::VirtioMmio,
    tap: std::fs::File,
    mac: [u8; 6],
}

impl VirtioNet {
    pub const MMIO_BASE: u64 = VIRTIO_NET_MMIO_BASE;
    pub const IRQ: u32 = VIRTIO_NET_IRQ;

    /// Open or create TAP `ifname` and register IRQ `VIRTIO_NET_IRQ` with KVM.
    pub fn new(ifname: &str, vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        let tap = open_tap(ifname)?;
        let mac = mac_from_name(ifname);

        let mmio = super::virtio_mmio::VirtioMmio::new(
            VIRTIO_NET_MMIO_BASE,
            VIRTIO_ID_NET,
            DEVICE_FEATURES,
            2,
            QUEUE_MAX_SIZE,
        )
        .map_err(crate::error::Error::Net)?;
        mmio.register_irq(vm, VIRTIO_NET_IRQ)
            .map_err(crate::error::Error::KvmIoctl)?;

        Ok(Self { mmio, tap, mac })
    }

    pub fn handles(&self, addr: u64) -> bool {
        self.mmio.handles(addr)
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        let cfg = self.config_space();
        self.mmio.read(addr, data, &cfg);
    }

    pub fn write(
        &mut self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        let notify = self
            .mmio
            .write(addr, data, mem)
            .map_err(crate::error::Error::Net)?;
        match notify {
            Some(QUEUE_RX) => self.poll_tap(mem)?,
            Some(QUEUE_TX) => self.process_tx(mem)?,
            _ => {}
        }
        Ok(())
    }

    /// Pull frames from the TAP into guest RX buffers when available.
    pub fn poll_tap(&mut self, mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        // Also drain TX in case a notify was coalesced / missed.
        self.process_tx(mem)?;

        let rx_ready = self
            .mmio
            .queue(QUEUE_RX)
            .is_some_and(virtio_queue::QueueT::ready);
        if !rx_ready {
            return Ok(());
        }

        let mut frame = [0u8; MAX_FRAME];
        loop {
            // With IFF_VNET_HDR, each read is virtio_net_hdr || eth frame.
            match self.tap.read(&mut frame) {
                Ok(0) => break,
                Ok(n) if n < NET_HDR_LEN => {
                    // Truncated; drop.
                }
                Ok(n) => {
                    if !self.deliver_rx(mem, &frame[..n])? {
                        // No RX buffer available; drop the frame.
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                Err(e) => return Err(crate::error::Error::Net(format!("tap read: {e}"))),
            }
        }
        Ok(())
    }

    fn config_space(&self) -> [u8; CONFIG_SIZE] {
        let mut cfg = [0u8; CONFIG_SIZE];
        cfg[..6].copy_from_slice(&self.mac);
        cfg[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
        cfg
    }

    fn process_tx(&mut self, mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        let tx_ready = self
            .mmio
            .queue(QUEUE_TX)
            .is_some_and(virtio_queue::QueueT::ready);
        if !tx_ready {
            return Ok(());
        }

        let mut used_any = false;
        loop {
            let chain = {
                let Some(q) = self.mmio.queue_mut(QUEUE_TX) else {
                    break;
                };
                match q.pop_descriptor_chain(mem) {
                    Some(c) => c,
                    None => break,
                }
            };
            let head = chain.head_index();
            let mut reader = virtio_queue::Reader::new(mem, chain)
                .map_err(|e| crate::error::Error::Net(e.to_string()))?;
            let total = reader.available_bytes();
            if total >= NET_HDR_LEN {
                let mut buf = vec![0u8; total];
                reader
                    .read_exact(&mut buf)
                    .map_err(|e| crate::error::Error::Net(e.to_string()))?;
                // TAP with IFF_VNET_HDR expects virtio_net_hdr || eth frame in one write.
                tap_write_packet(&mut self.tap, &buf)?;
            }
            self.mmio
                .queue_mut(QUEUE_TX)
                .ok_or_else(|| crate::error::Error::Net("missing tx queue".into()))?
                .add_used(mem, head, 0)
                .map_err(|e| crate::error::Error::Net(e.to_string()))?;
            used_any = true;
        }
        if used_any {
            self.mmio
                .signal_used_queue()
                .map_err(crate::error::Error::Net)?;
        }
        Ok(())
    }

    /// Returns true if a guest RX buffer was consumed (filled or discarded).
    fn deliver_rx(
        &mut self,
        mem: &vm_memory::GuestMemoryMmap<()>,
        frame: &[u8],
    ) -> crate::error::Result<bool> {
        let chain = {
            let Some(q) = self.mmio.queue_mut(QUEUE_RX) else {
                return Ok(false);
            };
            match q.pop_descriptor_chain(mem) {
                Some(c) => c,
                None => return Ok(false),
            }
        };
        let head = chain.head_index();
        let mut writer = virtio_queue::Writer::new(mem, chain)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        let used_len = if writer.available_bytes() < frame.len() {
            0
        } else {
            writer
                .write_all(frame)
                .map_err(|e| crate::error::Error::Net(e.to_string()))?;
            frame.len() as u32
        };
        self.mmio
            .queue_mut(QUEUE_RX)
            .ok_or_else(|| crate::error::Error::Net("missing rx queue".into()))?
            .add_used(mem, head, used_len)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        self.mmio
            .signal_used_queue()
            .map_err(crate::error::Error::Net)?;
        Ok(true)
    }
}

/// One TAP write is one packet; never use `write_all` (it can split packets).
fn tap_write_packet(tap: &mut std::fs::File, packet: &[u8]) -> crate::error::Result<()> {
    match tap.write(packet) {
        Ok(n) if n == packet.len() => Ok(()),
        Ok(n) => Err(crate::error::Error::Net(format!(
            "tap short write: {n}/{}",
            packet.len()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // Drop the frame if the host is not accepting.
            Ok(())
        }
        Err(e) if e.raw_os_error() == Some(libc::EINTR) => tap_write_packet(tap, packet),
        Err(e) => Err(crate::error::Error::Net(format!("tap write: {e}"))),
    }
}

/// Minimal `struct ifreq` layout for `TUNSETIFF` on Linux.
#[repr(C)]
struct IfReq {
    name: [libc::c_char; 16],
    flags: libc::c_short,
    _pad: [u8; 22],
}

fn open_tap(ifname: &str) -> crate::error::Result<std::fs::File> {
    if ifname.is_empty() || ifname.len() >= 16 {
        return Err(crate::error::Error::Net(
            "TAP interface name must be 1..15 bytes".into(),
        ));
    }
    // SAFETY: open a system device node.
    let fd = unsafe {
        libc::open(
            c"/dev/net/tun".as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(crate::error::Error::Net(format!(
            "open /dev/net/tun: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: fd is owned; wrap for cleanup on error paths.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };

    let mut ifr = IfReq {
        name: [0; 16],
        // VNET_HDR: kernel expects/provides virtio_net_hdr on each packet.
        flags: IFF_TAP | IFF_NO_PI | IFF_VNET_HDR,
        _pad: [0; 22],
    };
    for (i, b) in ifname.bytes().enumerate() {
        ifr.name[i] = b as libc::c_char;
    }

    // TUNSETIFF = _IOW('T', 202, int) on Linux x86_64.
    const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
    // TUNSETVNETHDRSZ = _IOW('T', 216, int)
    const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;

    // SAFETY: ioctl with a valid fd and ifreq for TUNSETIFF.
    let rc = unsafe { libc::ioctl(owned.as_raw_fd(), TUNSETIFF, &mut ifr) };
    if rc < 0 {
        return Err(crate::error::Error::Net(format!(
            "TUNSETIFF {ifname}: {}",
            std::io::Error::last_os_error()
        )));
    }

    let hdr_sz = NET_HDR_LEN as libc::c_int;
    // SAFETY: set virtio_net_hdr size for IFF_VNET_HDR.
    let rc = unsafe { libc::ioctl(owned.as_raw_fd(), TUNSETVNETHDRSZ, &hdr_sz) };
    if rc < 0 {
        return Err(crate::error::Error::Net(format!(
            "TUNSETVNETHDRSZ: {}",
            std::io::Error::last_os_error()
        )));
    }

    // SAFETY: OwnedFd is consumed into a File.
    Ok(unsafe { std::fs::File::from_raw_fd(owned.into_raw_fd()) })
}

fn mac_from_name(ifname: &str) -> [u8; 6] {
    // Locally administered unicast MAC based on the interface name.
    let mut mac = [0x52u8, 0x54, 0x00, 0x00, 0x00, 0x00];
    let mut h: u32 = 0x811c_9dc5;
    for b in ifname.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    mac[3] = ((h >> 16) & 0xff) as u8;
    mac[4] = ((h >> 8) & 0xff) as u8;
    mac[5] = (h & 0xff) as u8;
    mac
}
