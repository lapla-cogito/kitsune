//! Virtio-mmio network device (virtio 1.0 modern) backed by a host TAP.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _};
use virtio_queue::QueueT as _;

/// MMIO base for the virtio-net device (after virtio-blk window).
pub const VIRTIO_NET_MMIO_BASE: u64 = 0xd000_1000;
/// Size of the virtio-mmio register window (and config space).
pub const VIRTIO_NET_MMIO_SIZE: u64 = 0x1000;
/// GSI / IRQ line for virtio-net.
pub const VIRTIO_NET_IRQ: u32 = 6;

const MMIO_MAGIC: u32 = 0x7472_6976; // "virt"
const MMIO_VERSION: u32 = 2;
const VIRTIO_ID_NET: u32 = 1;
const VENDOR_ID: u32 = 0x10_00;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const DEVICE_FEATURES: u64 = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS;

const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;
const VIRTIO_MMIO_INT_VRING: u32 = 1;

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

// Register offsets (virtio-mmio modern)
const REG_MAGIC: u64 = 0x00;
const REG_VERSION: u64 = 0x04;
const REG_DEVICE_ID: u64 = 0x08;
const REG_VENDOR_ID: u64 = 0x0c;
const REG_DEVICE_FEATURES: u64 = 0x10;
const REG_DEVICE_FEATURES_SEL: u64 = 0x14;
const REG_DRIVER_FEATURES: u64 = 0x20;
const REG_DRIVER_FEATURES_SEL: u64 = 0x24;
const REG_QUEUE_SEL: u64 = 0x30;
const REG_QUEUE_NUM_MAX: u64 = 0x34;
const REG_QUEUE_NUM: u64 = 0x38;
const REG_QUEUE_READY: u64 = 0x44;
const REG_QUEUE_NOTIFY: u64 = 0x50;
const REG_INTERRUPT_STATUS: u64 = 0x60;
const REG_INTERRUPT_ACK: u64 = 0x64;
const REG_STATUS: u64 = 0x70;
const REG_QUEUE_DESC_LOW: u64 = 0x80;
const REG_QUEUE_DESC_HIGH: u64 = 0x84;
const REG_QUEUE_AVAIL_LOW: u64 = 0x90;
const REG_QUEUE_AVAIL_HIGH: u64 = 0x94;
const REG_QUEUE_USED_LOW: u64 = 0xa0;
const REG_QUEUE_USED_HIGH: u64 = 0xa4;
const REG_CONFIG_GENERATION: u64 = 0xfc;
const REG_CONFIG: u64 = 0x100;
const CONFIG_SIZE: u64 = 8; // mac[6] + status[2]

/// Virtio-mmio network device using a host TAP interface.
pub struct VirtioNet {
    base: u64,
    tap: std::fs::File,
    mac: [u8; 6],
    rx: virtio_queue::Queue,
    tx: virtio_queue::Queue,
    status: u8,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    interrupt_status: u32,
    irq_fd: vmm_sys_util::eventfd::EventFd,
}

impl VirtioNet {
    pub const MMIO_BASE: u64 = VIRTIO_NET_MMIO_BASE;
    pub const IRQ: u32 = VIRTIO_NET_IRQ;

    /// Open or create TAP `ifname` and register IRQ `VIRTIO_NET_IRQ` with KVM.
    pub fn new(ifname: &str, vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        let tap = open_tap(ifname)?;
        let mac = mac_from_name(ifname);

        let irq_fd = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        vm.register_irqfd(&irq_fd, VIRTIO_NET_IRQ)
            .map_err(crate::error::Error::KvmIoctl)?;

        let rx = virtio_queue::Queue::new(QUEUE_MAX_SIZE)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        let tx = virtio_queue::Queue::new(QUEUE_MAX_SIZE)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;

        Ok(Self {
            base: VIRTIO_NET_MMIO_BASE,
            tap,
            mac,
            rx,
            tx,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            interrupt_status: 0,
            irq_fd,
        })
    }

    pub fn handles(&self, addr: u64) -> bool {
        (self.base..self.base + VIRTIO_NET_MMIO_SIZE).contains(&addr)
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        let offset = addr - self.base;
        if (REG_CONFIG..REG_CONFIG + CONFIG_SIZE).contains(&offset) {
            let mut cfg = [0u8; CONFIG_SIZE as usize];
            cfg[..6].copy_from_slice(&self.mac);
            cfg[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
            for (i, byte) in data.iter_mut().enumerate() {
                let idx = (offset - REG_CONFIG) as usize + i;
                *byte = cfg.get(idx).copied().unwrap_or(0);
            }
            return;
        }
        write_le(data, self.read_reg(offset));
    }

    pub fn write(
        &mut self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        let offset = addr - self.base;
        let value = read_le(data);
        self.write_reg(offset, value, mem)
    }

    /// Pull frames from the TAP into guest RX buffers when available.
    pub fn poll_tap(&mut self, mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        // Also drain TX in case a notify was coalesced / missed.
        self.process_tx(mem)?;

        if !self.rx.ready() {
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

    fn read_reg(&self, offset: u64) -> u32 {
        match offset {
            REG_MAGIC => MMIO_MAGIC,
            REG_VERSION => MMIO_VERSION,
            REG_DEVICE_ID => VIRTIO_ID_NET,
            REG_VENDOR_ID => VENDOR_ID,
            REG_DEVICE_FEATURES => {
                if self.device_features_sel == 0 {
                    (DEVICE_FEATURES & 0xffff_ffff) as u32
                } else {
                    (DEVICE_FEATURES >> 32) as u32
                }
            }
            REG_QUEUE_NUM_MAX => match self.queue_sel {
                QUEUE_RX | QUEUE_TX => u32::from(QUEUE_MAX_SIZE),
                _ => 0,
            },
            REG_QUEUE_READY => u32::from(self.selected_queue().ready()),
            REG_INTERRUPT_STATUS => self.interrupt_status,
            REG_STATUS => u32::from(self.status),
            REG_CONFIG_GENERATION => 0,
            _ => 0,
        }
    }

    fn write_reg(
        &mut self,
        offset: u64,
        value: u32,
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | u64::from(value);
                } else {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | (u64::from(value) << 32);
                }
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM if self.is_data_queue() => {
                self.selected_queue_mut().set_size(value as u16);
            }
            REG_QUEUE_READY if self.is_data_queue() => {
                let ready = value == 1;
                self.selected_queue_mut().set_ready(ready);
                if ready && !self.selected_queue().is_valid(mem) {
                    self.selected_queue_mut().set_ready(false);
                }
            }
            REG_QUEUE_NOTIFY => match value {
                QUEUE_RX => self.poll_tap(mem)?,
                QUEUE_TX => self.process_tx(mem)?,
                _ => {}
            },
            REG_INTERRUPT_ACK => {
                self.interrupt_status &= !value;
            }
            REG_STATUS => {
                if value == 0 {
                    self.reset();
                } else {
                    self.status = value as u8;
                    if self.status & STATUS_FEATURES_OK != 0 {
                        let unknown = self.driver_features & !DEVICE_FEATURES;
                        if unknown != 0 || (self.driver_features & VIRTIO_F_VERSION_1) == 0 {
                            self.status &= !STATUS_FEATURES_OK;
                            self.status |= STATUS_FAILED;
                        }
                    }
                }
            }
            REG_QUEUE_DESC_LOW if self.is_data_queue() => {
                self.selected_queue_mut()
                    .set_desc_table_address(Some(value), None);
            }
            REG_QUEUE_DESC_HIGH if self.is_data_queue() => {
                self.selected_queue_mut()
                    .set_desc_table_address(None, Some(value));
            }
            REG_QUEUE_AVAIL_LOW if self.is_data_queue() => {
                self.selected_queue_mut()
                    .set_avail_ring_address(Some(value), None);
            }
            REG_QUEUE_AVAIL_HIGH if self.is_data_queue() => {
                self.selected_queue_mut()
                    .set_avail_ring_address(None, Some(value));
            }
            REG_QUEUE_USED_LOW if self.is_data_queue() => {
                self.selected_queue_mut()
                    .set_used_ring_address(Some(value), None);
            }
            REG_QUEUE_USED_HIGH if self.is_data_queue() => {
                self.selected_queue_mut()
                    .set_used_ring_address(None, Some(value));
            }
            _ => {}
        }
        Ok(())
    }

    fn is_data_queue(&self) -> bool {
        self.queue_sel == QUEUE_RX || self.queue_sel == QUEUE_TX
    }

    fn selected_queue(&self) -> &virtio_queue::Queue {
        if self.queue_sel == QUEUE_TX {
            &self.tx
        } else {
            &self.rx
        }
    }

    fn selected_queue_mut(&mut self) -> &mut virtio_queue::Queue {
        if self.queue_sel == QUEUE_TX {
            &mut self.tx
        } else {
            &mut self.rx
        }
    }

    fn reset(&mut self) {
        self.status = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.interrupt_status = 0;
        self.rx.reset();
        self.tx.reset();
    }

    fn process_tx(&mut self, mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        if !self.tx.ready() {
            return Ok(());
        }
        let mut used_any = false;
        while let Some(chain) = self.tx.pop_descriptor_chain(mem) {
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
            self.tx
                .add_used(mem, head, 0)
                .map_err(|e| crate::error::Error::Net(e.to_string()))?;
            used_any = true;
        }
        if used_any {
            self.signal_used_queue()?;
        }
        Ok(())
    }

    /// Returns true if a guest RX buffer was consumed (filled or discarded).
    fn deliver_rx(
        &mut self,
        mem: &vm_memory::GuestMemoryMmap<()>,
        frame: &[u8],
    ) -> crate::error::Result<bool> {
        let Some(chain) = self.rx.pop_descriptor_chain(mem) else {
            return Ok(false);
        };
        let head = chain.head_index();
        let mut writer = virtio_queue::Writer::new(mem, chain)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        if writer.available_bytes() < frame.len() {
            self.rx
                .add_used(mem, head, 0)
                .map_err(|e| crate::error::Error::Net(e.to_string()))?;
            self.signal_used_queue()?;
            return Ok(true);
        }
        writer
            .write_all(frame)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        self.rx
            .add_used(mem, head, frame.len() as u32)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        self.signal_used_queue()?;
        Ok(true)
    }

    fn signal_used_queue(&mut self) -> crate::error::Result<()> {
        self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
        self.irq_fd
            .write(1)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        Ok(())
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

fn read_le(data: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    let n = data.len().min(4);
    buf[..n].copy_from_slice(&data[..n]);
    u32::from_le_bytes(buf)
}

fn write_le(data: &mut [u8], value: u32) {
    let bytes = value.to_le_bytes();
    let n = data.len().min(4);
    data[..n].copy_from_slice(&bytes[..n]);
}
