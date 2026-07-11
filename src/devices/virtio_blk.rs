//! Virtio-mmio block device (virtio 1.0 modern transport).

use std::io::{Read as _, Seek as _, Write as _};
use virtio_queue::QueueT as _;

/// MMIO base used for the first (and only) virtio-blk device.
pub const VIRTIO_MMIO_BASE: u64 = 0xd000_0000;
/// Size of the virtio-mmio register window (and config space).
pub const VIRTIO_MMIO_SIZE: u64 = 0x1000;
/// GSI / IRQ line advertised to the guest for this device.
pub const VIRTIO_BLK_IRQ: u32 = 5;

const MMIO_MAGIC: u32 = 0x7472_6976; // "virt"
const MMIO_VERSION: u32 = 2;
const VIRTIO_ID_BLOCK: u32 = 2;
const VENDOR_ID: u32 = 0x10_00; // QEMU-compatible

// Feature bits
const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const DEVICE_FEATURES: u64 = VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_FLUSH;

// Status bits
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;

// Interrupt status bits
const VIRTIO_MMIO_INT_VRING: u32 = 1;

// Block request types / status
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const SECTOR_SIZE: u64 = 512;
const QUEUE_MAX_SIZE: u16 = 256;

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

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct VirtioBlkReqHdr {
    type_: u32,
    ioprio: u32,
    sector: u64,
}

// SAFETY: plain C layout, no padding concerns for ByteValued.
unsafe impl vm_memory::ByteValued for VirtioBlkReqHdr {}

/// Virtio-mmio block device backed by a host file.
pub struct VirtioBlock {
    base: u64,
    file: std::fs::File,
    capacity_sectors: u64,
    queue: virtio_queue::Queue,
    status: u8,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    interrupt_status: u32,
    irq_fd: vmm_sys_util::eventfd::EventFd,
}

impl VirtioBlock {
    /// MMIO base address of the device.
    pub const MMIO_BASE: u64 = VIRTIO_MMIO_BASE;
    /// Interrupt line of the device.
    pub const IRQ: u32 = VIRTIO_BLK_IRQ;

    /// Open `path` as a raw disk image and register IRQ `VIRTIO_BLK_IRQ` with KVM.
    pub fn new(path: &std::path::Path, vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(crate::error::Error::ImageIo)?;
        let len = file.metadata().map_err(crate::error::Error::ImageIo)?.len();
        if len < SECTOR_SIZE || !len.is_multiple_of(SECTOR_SIZE) {
            return Err(crate::error::Error::Block(
                "disk image size must be a non-zero multiple of 512 bytes".into(),
            ));
        }
        let capacity_sectors = len / SECTOR_SIZE;

        let irq_fd = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| crate::error::Error::Block(e.to_string()))?;
        vm.register_irqfd(&irq_fd, VIRTIO_BLK_IRQ)
            .map_err(crate::error::Error::KvmIoctl)?;

        let queue = virtio_queue::Queue::new(QUEUE_MAX_SIZE)
            .map_err(|e| crate::error::Error::Block(e.to_string()))?;

        Ok(Self {
            base: VIRTIO_MMIO_BASE,
            file,
            capacity_sectors,
            queue,
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
        (self.base..self.base + VIRTIO_MMIO_SIZE).contains(&addr)
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        let offset = addr - self.base;
        if (REG_CONFIG..REG_CONFIG + 8).contains(&offset) {
            let cap = self.capacity_sectors.to_le_bytes();
            for (i, byte) in data.iter_mut().enumerate() {
                let idx = (offset - REG_CONFIG) as usize + i;
                *byte = cap.get(idx).copied().unwrap_or(0);
            }
            return;
        }
        let value = self.read_reg(offset);
        write_le(data, value);
    }

    pub fn write(
        &mut self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        let offset = addr - self.base;
        let value = read_le(data);
        self.write_reg(offset, value, data.len(), mem)
    }

    fn read_reg(&self, offset: u64) -> u32 {
        match offset {
            REG_MAGIC => MMIO_MAGIC,
            REG_VERSION => MMIO_VERSION,
            REG_DEVICE_ID => VIRTIO_ID_BLOCK,
            REG_VENDOR_ID => VENDOR_ID,
            REG_DEVICE_FEATURES => {
                if self.device_features_sel == 0 {
                    (DEVICE_FEATURES & 0xffff_ffff) as u32
                } else {
                    (DEVICE_FEATURES >> 32) as u32
                }
            }
            REG_QUEUE_NUM_MAX => {
                if self.queue_sel == 0 {
                    u32::from(self.queue.max_size())
                } else {
                    0
                }
            }
            REG_QUEUE_READY => u32::from(self.queue.ready()),
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
        access_len: usize,
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
            REG_QUEUE_NUM => {
                if self.queue_sel == 0 {
                    self.queue.set_size(value as u16);
                }
            }
            REG_QUEUE_READY => {
                if self.queue_sel == 0 {
                    let ready = value == 1;
                    self.queue.set_ready(ready);
                    // `is_valid` requires ready == true, so check after enabling.
                    if ready && !self.queue.is_valid(mem) {
                        self.queue.set_ready(false);
                    }
                }
            }
            REG_QUEUE_NOTIFY => {
                if value == 0 {
                    self.process_queue(mem)?;
                }
            }
            REG_INTERRUPT_ACK => {
                self.interrupt_status &= !value;
            }
            REG_STATUS => {
                if value == 0 {
                    self.reset();
                } else {
                    self.status = value as u8;
                    // Validate negotiated features once FEATURES_OK is set.
                    if self.status & STATUS_FEATURES_OK != 0 {
                        let unknown = self.driver_features & !DEVICE_FEATURES;
                        if unknown != 0 || (self.driver_features & VIRTIO_F_VERSION_1) == 0 {
                            self.status &= !STATUS_FEATURES_OK;
                            self.status |= STATUS_FAILED;
                        }
                    }
                }
            }
            REG_QUEUE_DESC_LOW => {
                if self.queue_sel == 0 {
                    self.queue.set_desc_table_address(Some(value), None);
                }
            }
            REG_QUEUE_DESC_HIGH => {
                if self.queue_sel == 0 {
                    self.queue.set_desc_table_address(None, Some(value));
                }
            }
            REG_QUEUE_AVAIL_LOW => {
                if self.queue_sel == 0 {
                    self.queue.set_avail_ring_address(Some(value), None);
                }
            }
            REG_QUEUE_AVAIL_HIGH => {
                if self.queue_sel == 0 {
                    self.queue.set_avail_ring_address(None, Some(value));
                }
            }
            REG_QUEUE_USED_LOW => {
                if self.queue_sel == 0 {
                    self.queue.set_used_ring_address(Some(value), None);
                }
            }
            REG_QUEUE_USED_HIGH => {
                if self.queue_sel == 0 {
                    self.queue.set_used_ring_address(None, Some(value));
                }
            }
            o if (REG_CONFIG..REG_CONFIG + 8).contains(&o) => {
                // config is read-only
                let _ = access_len;
            }
            _ => {}
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.status = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.interrupt_status = 0;
        self.queue.reset();
    }

    fn process_queue(&mut self, mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        if !self.queue.ready() {
            return Ok(());
        }

        let mut used_any = false;
        while let Some(chain) = self.queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            // Status byte is written inside `handle_request` on all paths that can.
            let written = self.handle_request(mem, chain).unwrap_or(1);
            self.queue
                .add_used(mem, head, written)
                .map_err(|e| crate::error::Error::Block(e.to_string()))?;
            used_any = true;
        }

        if used_any {
            self.signal_used_queue()?;
        }
        Ok(())
    }

    fn handle_request(
        &mut self,
        mem: &vm_memory::GuestMemoryMmap<()>,
        chain: virtio_queue::DescriptorChain<&vm_memory::GuestMemoryMmap<()>>,
    ) -> Result<u32, BlockReqError> {
        let mut reader =
            virtio_queue::Reader::new(mem, chain.clone()).map_err(|_| BlockReqError::Chain)?;
        let mut writer = virtio_queue::Writer::new(mem, chain).map_err(|_| BlockReqError::Chain)?;

        let hdr: VirtioBlkReqHdr = reader.read_obj().map_err(|_| BlockReqError::Io)?;

        match hdr.type_ {
            VIRTIO_BLK_T_IN => {
                // Writable region = data + status byte.
                let total = writer.available_bytes();
                if total == 0 {
                    return Err(BlockReqError::Chain);
                }
                let data_len = total - 1;
                let offset = hdr
                    .sector
                    .checked_mul(SECTOR_SIZE)
                    .ok_or(BlockReqError::Io)?;
                let end = offset
                    .checked_add(data_len as u64)
                    .ok_or(BlockReqError::Io)?;
                let mut status = VIRTIO_BLK_S_OK;
                let mut buf = vec![0u8; data_len];
                if end > self.capacity_sectors * SECTOR_SIZE {
                    status = VIRTIO_BLK_S_IOERR;
                } else if self
                    .file
                    .seek(std::io::SeekFrom::Start(offset))
                    .and_then(|_| self.file.read_exact(&mut buf))
                    .is_err()
                {
                    buf.fill(0);
                    status = VIRTIO_BLK_S_IOERR;
                }
                writer.write_all(&buf).map_err(|_| BlockReqError::Io)?;
                writer.write_all(&[status]).map_err(|_| BlockReqError::Io)?;
                Ok((data_len + 1) as u32)
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                let mut buf = vec![0u8; data_len];
                reader.read_exact(&mut buf).map_err(|_| BlockReqError::Io)?;
                let offset = hdr
                    .sector
                    .checked_mul(SECTOR_SIZE)
                    .ok_or(BlockReqError::Io)?;
                let end = offset
                    .checked_add(data_len as u64)
                    .ok_or(BlockReqError::Io)?;
                let out_of_range = end > self.capacity_sectors * SECTOR_SIZE;
                let io_err = !out_of_range
                    && self
                        .file
                        .seek(std::io::SeekFrom::Start(offset))
                        .and_then(|_| self.file.write_all(&buf))
                        .is_err();
                let status = if out_of_range || io_err {
                    VIRTIO_BLK_S_IOERR
                } else {
                    VIRTIO_BLK_S_OK
                };
                writer.write_all(&[status]).map_err(|_| BlockReqError::Io)?;
                Ok(1)
            }
            VIRTIO_BLK_T_FLUSH => {
                self.file.sync_all().map_err(|_| BlockReqError::Io)?;
                writer
                    .write_all(&[VIRTIO_BLK_S_OK])
                    .map_err(|_| BlockReqError::Io)?;
                Ok(1)
            }
            VIRTIO_BLK_T_GET_ID => {
                let mut id = [0u8; 20];
                let name = b"kitsune-blk";
                id[..name.len()].copy_from_slice(name);
                let total = writer.available_bytes();
                if total < 21 {
                    return Err(BlockReqError::Chain);
                }
                writer.write_all(&id).map_err(|_| BlockReqError::Io)?;
                writer
                    .write_all(&[VIRTIO_BLK_S_OK])
                    .map_err(|_| BlockReqError::Io)?;
                Ok(21)
            }
            _ => {
                let total = writer.available_bytes();
                if total == 0 {
                    return Err(BlockReqError::Unsupported);
                }
                if total > 1 {
                    let _ = writer.write_all(&vec![0u8; total - 1]);
                }
                let _ = writer.write_all(&[VIRTIO_BLK_S_UNSUPP]);
                Ok(total as u32)
            }
        }
    }

    fn signal_used_queue(&mut self) -> crate::error::Result<()> {
        self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
        self.irq_fd
            .write(1)
            .map_err(|e| crate::error::Error::Block(e.to_string()))?;
        Ok(())
    }
}

enum BlockReqError {
    Io,
    Chain,
    Unsupported,
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
