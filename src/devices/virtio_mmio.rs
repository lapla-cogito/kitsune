//! Virtio-mmio transport.

use virtio_queue::QueueT as _;

/// Size of one virtio-mmio register window (including config space).
pub const MMIO_SIZE: u64 = 0x1000;

/// Transport feature: virtio 1.0+.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const MMIO_MAGIC: u32 = 0x7472_6976; // "virt"
const MMIO_VERSION: u32 = 2;
const VENDOR_ID: u32 = 0x10_00;

const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;
const VIRTIO_MMIO_INT_VRING: u32 = 1;

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
/// Guest writes the queue index here to notify the device.
pub const REG_QUEUE_NOTIFY: u64 = 0x50;
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
/// Start of device-specific config space within the MMIO window.
pub const REG_CONFIG: u64 = 0x100;

/// Virtio-mmio transport state: registers, queues, and IRQ signalling.
pub struct VirtioMmio {
    base: u64,
    device_id: u32,
    device_features: u64,
    status: u8,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    interrupt_status: u32,
    irq_fd: vmm_sys_util::eventfd::EventFd,
    /// Signaled by KVM ioeventfd on `QueueNotify` writes (and software kicks).
    notify_fd: vmm_sys_util::eventfd::EventFd,
    queues: Vec<virtio_queue::Queue>,
}

impl VirtioMmio {
    /// Create transport registers and queues.
    /// Call [`register_irq`] and [`register_ioeventfds`] before the guest runs.
    pub fn new(
        base: u64,
        device_id: u32,
        device_features: u64,
        num_queues: usize,
        queue_max_size: u16,
    ) -> Result<Self, String> {
        if num_queues == 0 {
            return Err("virtio-mmio requires at least one queue".into());
        }
        let irq_fd =
            vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).map_err(|e| e.to_string())?;
        let notify_fd =
            vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).map_err(|e| e.to_string())?;

        let mut queues = Vec::with_capacity(num_queues);
        for _ in 0..num_queues {
            queues.push(virtio_queue::Queue::new(queue_max_size).map_err(|e| e.to_string())?);
        }

        Ok(Self {
            base,
            device_id,
            device_features,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            interrupt_status: 0,
            irq_fd,
            notify_fd,
            queues,
        })
    }

    /// Wire the transport irqfd to GSI `irq` on the VM.
    pub fn register_irq(&self, vm: &kvm_ioctls::VmFd, irq: u32) -> Result<(), kvm_ioctls::Error> {
        vm.register_irqfd(&self.irq_fd, irq)
    }

    /// Register KVM ioeventfds so guest writes to `QueueNotify` signal [`notify_fd`].
    pub fn register_ioeventfds(&self, vm: &kvm_ioctls::VmFd) -> Result<(), kvm_ioctls::Error> {
        let addr = kvm_ioctls::IoEventAddress::Mmio(self.base + REG_QUEUE_NOTIFY);
        for index in 0..self.queues.len() {
            vm.register_ioevent(&self.notify_fd, &addr, index as u32)?;
        }
        Ok(())
    }

    /// EventFd that fires when the guest notifies any queue via ioeventfd.
    /// Devices also write this fd for software kick / stop and MMIO notify fallback.
    pub fn notify_fd(&self) -> &vmm_sys_util::eventfd::EventFd {
        &self.notify_fd
    }

    pub fn driver_features(&self) -> u64 {
        self.driver_features
    }

    pub fn queue(&self, index: u32) -> Option<&virtio_queue::Queue> {
        self.queues.get(index as usize)
    }

    pub fn queue_mut(&mut self, index: u32) -> Option<&mut virtio_queue::Queue> {
        self.queues.get_mut(index as usize)
    }

    /// Read MMIO registers or device config (`config` is little-endian device layout).
    pub fn read(&self, addr: u64, data: &mut [u8], config: &[u8]) {
        let offset = addr - self.base;
        let config_end = REG_CONFIG + config.len() as u64;
        if (REG_CONFIG..config_end).contains(&offset) {
            for (i, byte) in data.iter_mut().enumerate() {
                let idx = (offset - REG_CONFIG) as usize + i;
                *byte = config.get(idx).copied().unwrap_or(0);
            }
            return;
        }
        write_le(data, self.read_reg(offset));
    }

    /// Write MMIO registers.
    pub fn write(
        &mut self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> Result<Option<u32>, String> {
        let offset = addr - self.base;
        let value = read_le(data);
        self.write_reg(offset, value, mem)
    }

    pub fn signal_used_queue(&mut self) -> Result<(), String> {
        self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
        self.irq_fd.write(1).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn reset(&mut self) {
        self.status = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.interrupt_status = 0;
        for q in &mut self.queues {
            q.reset();
        }
    }

    fn selected_queue(&self) -> Option<&virtio_queue::Queue> {
        self.queues.get(self.queue_sel as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut virtio_queue::Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    fn read_reg(&self, offset: u64) -> u32 {
        match offset {
            REG_MAGIC => MMIO_MAGIC,
            REG_VERSION => MMIO_VERSION,
            REG_DEVICE_ID => self.device_id,
            REG_VENDOR_ID => VENDOR_ID,
            REG_DEVICE_FEATURES => match self.device_features_sel {
                0 => (self.device_features & 0xffff_ffff) as u32,
                1 => (self.device_features >> 32) as u32,
                _ => 0,
            },
            REG_QUEUE_NUM_MAX => self
                .selected_queue()
                .map(|q| u32::from(q.max_size()))
                .unwrap_or(0),
            REG_QUEUE_READY => self
                .selected_queue()
                .map(|q| u32::from(q.ready()))
                .unwrap_or(0),
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
    ) -> Result<Option<u32>, String> {
        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES => match self.driver_features_sel {
                0 => {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | u64::from(value);
                }
                1 => {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | (u64::from(value) << 32);
                }
                _ => {}
            },
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM => {
                if let Some(q) = self.selected_queue_mut() {
                    q.set_size(value as u16);
                }
            }
            REG_QUEUE_READY => {
                if let Some(q) = self.selected_queue_mut() {
                    let ready = value == 1;
                    q.set_ready(ready);
                    if ready && !q.is_valid(mem) {
                        q.set_ready(false);
                    }
                }
            }
            REG_QUEUE_NOTIFY => {
                if (value as usize) < self.queues.len() {
                    return Ok(Some(value));
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
                    if self.status & STATUS_FEATURES_OK != 0 {
                        let unknown = self.driver_features & !self.device_features;
                        if unknown != 0 || (self.driver_features & VIRTIO_F_VERSION_1) == 0 {
                            self.status &= !STATUS_FEATURES_OK;
                            self.status |= STATUS_FAILED;
                        }
                    }
                }
            }
            REG_QUEUE_DESC_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.set_desc_table_address(Some(value), None);
                }
            }
            REG_QUEUE_DESC_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.set_desc_table_address(None, Some(value));
                }
            }
            REG_QUEUE_AVAIL_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.set_avail_ring_address(Some(value), None);
                }
            }
            REG_QUEUE_AVAIL_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.set_avail_ring_address(None, Some(value));
                }
            }
            REG_QUEUE_USED_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.set_used_ring_address(Some(value), None);
                }
            }
            REG_QUEUE_USED_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.set_used_ring_address(None, Some(value));
                }
            }
            _ => {}
        }
        Ok(None)
    }
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

#[cfg(test)]
mod tests {
    #[test]
    fn feature_words_split() {
        let feats = super::VIRTIO_F_VERSION_1 | 0x21;
        assert_eq!((feats & 0xffff_ffff) as u32, 0x21);
        assert_eq!((feats >> 32) as u32, 1);
    }

    #[test]
    fn read_le_partial() {
        assert_eq!(super::read_le(&[0x78, 0x56]), 0x0000_5678);
        assert_eq!(super::read_le(&[1, 2, 3, 4]), 0x0403_0201);
    }
}
