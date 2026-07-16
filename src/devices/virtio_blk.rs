//! Virtio-mmio block device (virtio 1.0 modern transport).

use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use virtio_queue::QueueT as _;

/// MMIO base used for the first (and only) virtio-blk device.
pub const VIRTIO_MMIO_BASE: u64 = 0xd000_0000;
/// GSI / IRQ line advertised to the guest for this device.
pub const VIRTIO_BLK_IRQ: u32 = 5;

const VIRTIO_ID_BLOCK: u32 = 2;

const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const CONFIG_SIZE_MAX: u32 = 65536;
/// Max data descriptors per request (header + status are extra).
const CONFIG_SEG_MAX: u32 = 32;
const CONFIG_BLK_SIZE: u32 = 512;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const SECTOR_SIZE: u64 = 512;
const QUEUE_MAX_SIZE: u16 = 256;
const QUEUE_REQUEST: u32 = 0;

/// Bytes of `struct virtio_blk_config` we expose (through `blk_size`).
const CONFIG_LEN: usize = 24;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct VirtioBlkReqHdr {
    type_: u32,
    ioprio: u32,
    sector: u64,
}

// SAFETY: plain C layout, no padding concerns for ByteValued.
unsafe impl vm_memory::ByteValued for VirtioBlkReqHdr {}

/// Transport and static geometry. MMIO handlers and the worker share this lock.
/// Host file I/O uses [`VirtioBlock::file`] so disk latency does not block MMIO.
struct BlockTransport {
    mmio: super::virtio_mmio::VirtioMmio,
    capacity_sectors: u64,
    readonly: bool,
}

/// Virtio-mmio block device backed by a host file.
pub struct VirtioBlock {
    base: u64,
    transport: std::sync::Arc<std::sync::Mutex<BlockTransport>>,
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    /// Wakes the worker on queue notify (ioeventfd / MMIO fallback) and stop.
    kick: vmm_sys_util::eventfd::EventFd,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<crate::error::Result<()>>>>,
}

impl VirtioBlock {
    /// MMIO base address of the device.
    pub const MMIO_BASE: u64 = VIRTIO_MMIO_BASE;
    /// Interrupt line of the device.
    pub const IRQ: u32 = VIRTIO_BLK_IRQ;

    /// Open `path` as a raw disk image and register IRQ / ioeventfd with KVM.
    pub fn new(path: &std::path::Path, vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        let (file, readonly) = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(f) => (f, false),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .open(path)
                    .map_err(crate::error::Error::ImageIo)?;
                (f, true)
            }
            Err(e) => return Err(crate::error::Error::ImageIo(e)),
        };

        let len = file.metadata().map_err(crate::error::Error::ImageIo)?.len();
        if len < SECTOR_SIZE || !len.is_multiple_of(SECTOR_SIZE) {
            return Err(crate::error::Error::Block(
                "disk image size must be a non-zero multiple of 512 bytes".into(),
            ));
        }
        let capacity_sectors = len / SECTOR_SIZE;

        let mmio = super::virtio_mmio::VirtioMmio::new(
            VIRTIO_MMIO_BASE,
            VIRTIO_ID_BLOCK,
            advertised_features(readonly),
            1,
            QUEUE_MAX_SIZE,
        )
        .map_err(crate::error::Error::Block)?;
        mmio.register_irq(vm, VIRTIO_BLK_IRQ)
            .map_err(crate::error::Error::KvmIoctl)?;
        mmio.register_ioeventfds(vm)
            .map_err(crate::error::Error::KvmIoctl)?;

        let kick = mmio
            .notify_fd()
            .try_clone()
            .map_err(|e| crate::error::Error::Block(e.to_string()))?;

        Ok(Self {
            base: VIRTIO_MMIO_BASE,
            transport: std::sync::Arc::new(std::sync::Mutex::new(BlockTransport {
                mmio,
                capacity_sectors,
                readonly,
            })),
            file: std::sync::Arc::new(std::sync::Mutex::new(file)),
            kick,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker: std::sync::Mutex::new(None),
        })
    }

    /// Start the queue worker (polls the QueueNotify ioeventfd).
    pub fn start_worker(&self, mem: vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        let mut slot = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Ok(());
        }

        self.stop.store(false, std::sync::atomic::Ordering::SeqCst);
        let transport = std::sync::Arc::clone(&self.transport);
        let file = std::sync::Arc::clone(&self.file);
        let kick = self
            .kick
            .try_clone()
            .map_err(|e| crate::error::Error::Block(e.to_string()))?;
        let stop = std::sync::Arc::clone(&self.stop);

        let handle = std::thread::Builder::new()
            .name("virtio-blk".into())
            .spawn(move || worker_loop(transport, file, kick, mem, stop))
            .map_err(|e| crate::error::Error::Block(e.to_string()))?;
        *slot = Some(handle);
        Ok(())
    }

    /// Stop the worker and wait for it to exit.
    pub fn stop_worker(&self) -> crate::error::Result<()> {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.kick.write(1);
        let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(());
        };
        match handle.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(crate::error::Error::Block(
                "virtio-blk worker panicked".into(),
            )),
        }
    }

    pub fn handles(&self, addr: u64) -> bool {
        (self.base..self.base + super::virtio_mmio::MMIO_SIZE).contains(&addr)
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        let transport = self.transport.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = config_space(transport.capacity_sectors);
        transport.mmio.read(addr, data, &cfg);
    }

    pub fn write(
        &self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        let notify = {
            let mut transport = self.transport.lock().unwrap_or_else(|e| e.into_inner());
            transport
                .mmio
                .write(addr, data, mem)
                .map_err(crate::error::Error::Block)?
        };
        if notify.is_some() {
            self.kick
                .write(1)
                .map_err(|e| crate::error::Error::Block(e.to_string()))?;
        }
        Ok(())
    }
}

impl Drop for VirtioBlock {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

fn worker_loop(
    transport: std::sync::Arc<std::sync::Mutex<BlockTransport>>,
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    kick: vmm_sys_util::eventfd::EventFd,
    mem: vm_memory::GuestMemoryMmap<()>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> crate::error::Result<()> {
    let kick_fd = kick.as_raw_fd();

    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let mut fds = [libc::pollfd {
            fd: kick_fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: poll on a valid eventfd owned for the worker lifetime.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 50) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(crate::error::Error::Block(format!("poll: {err}")));
        }

        if fds[0].revents == 0 {
            continue;
        }
        let _ = kick.read();

        if let Err(e) = process_queue(&transport, &file, &mem) {
            eprintln!("kitsune: virtio-blk: {e}");
        }
    }
    Ok(())
}

fn process_queue(
    transport: &std::sync::Mutex<BlockTransport>,
    file: &std::sync::Mutex<std::fs::File>,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<()> {
    let mut used_any = false;
    loop {
        let popped = {
            let mut t = transport.lock().unwrap_or_else(|e| e.into_inner());
            if !t
                .mmio
                .queue(QUEUE_REQUEST)
                .is_some_and(virtio_queue::QueueT::ready)
            {
                None
            } else {
                t.mmio
                    .queue_mut(QUEUE_REQUEST)
                    .and_then(|q| q.pop_descriptor_chain(mem))
                    .map(|c| {
                        let head = c.head_index();
                        (
                            head,
                            c,
                            t.capacity_sectors,
                            t.readonly,
                            t.mmio.driver_features(),
                        )
                    })
            }
        };
        let Some((head, chain, capacity_sectors, readonly, driver_features)) = popped else {
            break;
        };

        let written = {
            let mut f = file.lock().unwrap_or_else(|e| e.into_inner());
            handle_request(
                &mut f,
                capacity_sectors,
                readonly,
                driver_features,
                mem,
                chain,
            )
            .unwrap_or(1)
        };

        {
            let mut t = transport.lock().unwrap_or_else(|e| e.into_inner());
            t.mmio
                .queue_mut(QUEUE_REQUEST)
                .ok_or_else(|| crate::error::Error::Block("missing request queue".into()))?
                .add_used(mem, head, written)
                .map_err(|e| crate::error::Error::Block(e.to_string()))?;
        }
        used_any = true;
    }

    if used_any {
        let mut t = transport.lock().unwrap_or_else(|e| e.into_inner());
        t.mmio
            .signal_used_queue()
            .map_err(crate::error::Error::Block)?;
    }
    Ok(())
}

fn handle_request(
    file: &mut std::fs::File,
    capacity_sectors: u64,
    readonly: bool,
    driver_features: u64,
    mem: &vm_memory::GuestMemoryMmap<()>,
    chain: virtio_queue::DescriptorChain<&vm_memory::GuestMemoryMmap<()>>,
) -> Result<u32, BlockReqError> {
    let mut reader =
        virtio_queue::Reader::new(mem, chain.clone()).map_err(|_| BlockReqError::Chain)?;
    let mut writer = virtio_queue::Writer::new(mem, chain).map_err(|_| BlockReqError::Chain)?;

    let hdr: VirtioBlkReqHdr = reader.read_obj().map_err(|_| BlockReqError::Io)?;

    match hdr.type_ {
        VIRTIO_BLK_T_IN => {
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
            if end > capacity_sectors * SECTOR_SIZE {
                status = VIRTIO_BLK_S_IOERR;
            } else if file
                .seek(std::io::SeekFrom::Start(offset))
                .and_then(|_| file.read_exact(&mut buf))
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

            let readonly = readonly || (driver_features & VIRTIO_BLK_F_RO) != 0;
            let out_of_range = end > capacity_sectors * SECTOR_SIZE;
            let io_err = !readonly
                && !out_of_range
                && file
                    .seek(std::io::SeekFrom::Start(offset))
                    .and_then(|_| file.write_all(&buf))
                    .is_err();
            let status = if readonly || out_of_range || io_err {
                VIRTIO_BLK_S_IOERR
            } else {
                VIRTIO_BLK_S_OK
            };
            writer.write_all(&[status]).map_err(|_| BlockReqError::Io)?;
            Ok(1)
        }
        VIRTIO_BLK_T_FLUSH => {
            let status = if (driver_features & VIRTIO_BLK_F_FLUSH) == 0 {
                VIRTIO_BLK_S_UNSUPP
            } else if readonly {
                VIRTIO_BLK_S_OK
            } else if file.sync_data().is_err() {
                VIRTIO_BLK_S_IOERR
            } else {
                VIRTIO_BLK_S_OK
            };
            writer.write_all(&[status]).map_err(|_| BlockReqError::Io)?;
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

enum BlockReqError {
    Io,
    Chain,
    Unsupported,
}

fn advertised_features(readonly: bool) -> u64 {
    let mut feats = super::virtio_mmio::VIRTIO_F_VERSION_1
        | VIRTIO_BLK_F_SIZE_MAX
        | VIRTIO_BLK_F_SEG_MAX
        | VIRTIO_BLK_F_BLK_SIZE
        | VIRTIO_BLK_F_FLUSH;
    if readonly {
        feats |= VIRTIO_BLK_F_RO;
    }
    feats
}

fn config_space(capacity_sectors: u64) -> [u8; CONFIG_LEN] {
    let mut cfg = [0u8; CONFIG_LEN];
    cfg[0..8].copy_from_slice(&capacity_sectors.to_le_bytes());
    cfg[8..12].copy_from_slice(&CONFIG_SIZE_MAX.to_le_bytes());
    cfg[12..16].copy_from_slice(&CONFIG_SEG_MAX.to_le_bytes());
    cfg[20..24].copy_from_slice(&CONFIG_BLK_SIZE.to_le_bytes());
    cfg
}

#[cfg(test)]
mod tests {
    #[test]
    fn features_include_flush_and_optional_ro() {
        let rw = super::advertised_features(false);
        assert_ne!(rw & super::VIRTIO_BLK_F_FLUSH, 0);
        assert_ne!(rw & (1u64 << 32), 0);
        assert_eq!(rw & super::VIRTIO_BLK_F_RO, 0);

        let ro = super::advertised_features(true);
        assert_ne!(ro & super::VIRTIO_BLK_F_RO, 0);
    }

    #[test]
    fn config_layout_capacity_and_blk_size() {
        let sectors = 2048u64;
        let cfg = super::config_space(sectors);
        assert_eq!(&cfg[0..8], &sectors.to_le_bytes());
        assert_eq!(&cfg[8..12], &super::CONFIG_SIZE_MAX.to_le_bytes());
        assert_eq!(&cfg[12..16], &super::CONFIG_SEG_MAX.to_le_bytes());
        assert_eq!(&cfg[20..24], &super::CONFIG_BLK_SIZE.to_le_bytes());
    }
}
