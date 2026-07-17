//! Virtio-mmio block device (virtio 1.0 modern transport).

use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
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

/// Alignment for the scratch buffer.
const IO_ALIGN: usize = 4096;
/// Initial scratch capacity.
const SCRATCH_INITIAL: usize = 64 * 1024;

const RING_USER_READ: u64 = 1;
const RING_USER_WRITE: u64 = 2;
const RING_USER_FSYNC: u64 = 3;

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
struct BlockTransport {
    mmio: super::virtio_mmio::VirtioMmio,
    capacity_sectors: u64,
    readonly: bool,
}

/// Host disk handles used only on the block worker thread.
struct BlockDisk {
    /// Preferred path when `use_direct` and the transfer meets [`direct_align`].
    direct_file: Option<std::fs::File>,
    /// Used for unaligned I/O and as fallback.
    buffered_file: std::fs::File,
    /// When false, always use [`buffered_file`].
    use_direct: bool,
    /// Host O_DIRECT alignment from `st_blksize` (at least [`SECTOR_SIZE`]).
    direct_align: u64,
    scratch: AlignedBuf,
    ring: Option<io_uring::IoUring>,
}

/// Page-aligned reusable host buffer.
struct AlignedBuf {
    ptr: *mut u8,
    cap: usize,
}

// SAFETY: exclusive use on the block worker thread.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    fn new(cap: usize) -> Result<Self, String> {
        let cap = cap.max(IO_ALIGN).next_multiple_of(IO_ALIGN);
        let layout =
            std::alloc::Layout::from_size_align(cap, IO_ALIGN).map_err(|e| e.to_string())?;
        // SAFETY: layout has non-zero size and valid alignment.
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            return Err("aligned buffer allocation failed".into());
        }
        Ok(Self { ptr, cap })
    }

    fn ensure(&mut self, need: usize) -> Result<(), String> {
        if need <= self.cap {
            return Ok(());
        }
        let new_cap = need.next_multiple_of(IO_ALIGN);
        let old_layout =
            std::alloc::Layout::from_size_align(self.cap, IO_ALIGN).map_err(|e| e.to_string())?;
        let new_layout =
            std::alloc::Layout::from_size_align(new_cap, IO_ALIGN).map_err(|e| e.to_string())?;
        // SAFETY: ptr was allocated with old_layout; new size is larger.
        let new_ptr = unsafe { std::alloc::realloc(self.ptr, old_layout, new_layout.size()) };
        if new_ptr.is_null() {
            return Err("aligned buffer reallocation failed".into());
        }
        self.ptr = new_ptr;
        self.cap = new_cap;
        Ok(())
    }

    fn as_mut(&mut self, len: usize) -> Result<&mut [u8], String> {
        self.ensure(len)?;
        // SAFETY: ptr is valid for `cap` bytes; len <= cap after ensure.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.ptr, len) })
    }

    fn as_slice(&self, len: usize) -> Result<&[u8], String> {
        if len > self.cap {
            return Err("scratch buffer too small".into());
        }
        // SAFETY: ptr is valid for `cap` bytes.
        Ok(unsafe { std::slice::from_raw_parts(self.ptr, len) })
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if self.cap == 0 || self.ptr.is_null() {
            return;
        }
        // SAFETY: matches allocation in `new` / `ensure`.
        unsafe {
            let layout = std::alloc::Layout::from_size_align_unchecked(self.cap, IO_ALIGN);
            std::alloc::dealloc(self.ptr, layout);
        }
    }
}

/// Result of a finished worker.
struct WorkerOutcome {
    result: crate::error::Result<()>,
    disk: BlockDisk,
}

/// Virtio-mmio block device backed by a host file.
pub struct VirtioBlock {
    base: u64,
    transport: std::sync::Arc<std::sync::Mutex<BlockTransport>>,
    /// Taken by the worker at start. Restored when the worker stops.
    disk: std::sync::Mutex<Option<BlockDisk>>,
    /// Wakes the worker on queue notify (ioeventfd / MMIO fallback) and stop.
    kick: vmm_sys_util::eventfd::EventFd,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<WorkerOutcome>>>,
}

impl VirtioBlock {
    /// MMIO base address of the device.
    pub const MMIO_BASE: u64 = VIRTIO_MMIO_BASE;
    /// Interrupt line of the device.
    pub const IRQ: u32 = VIRTIO_BLK_IRQ;

    /// Open `path` as a raw disk image and register IRQ / ioeventfd with KVM.
    pub fn new(path: &std::path::Path, vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        let opened = open_disk_image(path)?;
        let len = opened
            .buffered_file
            .metadata()
            .map_err(crate::error::Error::ImageIo)?
            .len();
        if len < SECTOR_SIZE || !len.is_multiple_of(SECTOR_SIZE) {
            return Err(crate::error::Error::Block(
                "disk image size must be a non-zero multiple of 512 bytes".into(),
            ));
        }
        let capacity_sectors = len / SECTOR_SIZE;

        let mmio = super::virtio_mmio::VirtioMmio::new(
            VIRTIO_MMIO_BASE,
            VIRTIO_ID_BLOCK,
            advertised_features(opened.readonly),
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

        let scratch = AlignedBuf::new(SCRATCH_INITIAL).map_err(crate::error::Error::Block)?;
        let ring = io_uring::IoUring::new(8).ok();
        let use_direct = opened.direct_file.is_some();

        Ok(Self {
            base: VIRTIO_MMIO_BASE,
            transport: std::sync::Arc::new(std::sync::Mutex::new(BlockTransport {
                mmio,
                capacity_sectors,
                readonly: opened.readonly,
            })),
            disk: std::sync::Mutex::new(Some(BlockDisk {
                direct_file: opened.direct_file,
                buffered_file: opened.buffered_file,
                use_direct,
                direct_align: opened.direct_align,
                scratch,
                ring,
            })),
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

        let disk = self
            .disk
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| crate::error::Error::Block("block disk already in use".into()))?;

        // Keep disk reclaimable if spawn fails (closure drop would otherwise destroy it).
        let disk_slot = std::sync::Arc::new(std::sync::Mutex::new(Some(disk)));
        let disk_for_worker = std::sync::Arc::clone(&disk_slot);

        self.stop.store(false, std::sync::atomic::Ordering::SeqCst);
        let transport = std::sync::Arc::clone(&self.transport);
        let kick = match self.kick.try_clone() {
            Ok(k) => k,
            Err(e) => {
                *self.disk.lock().unwrap_or_else(|e| e.into_inner()) =
                    disk_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                return Err(crate::error::Error::Block(e.to_string()));
            }
        };
        let stop = std::sync::Arc::clone(&self.stop);

        let handle = std::thread::Builder::new()
            .name("virtio-blk".into())
            .spawn(move || {
                let disk = disk_for_worker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .expect("block disk present for worker");
                worker_loop(transport, disk, kick, mem, stop)
            });

        match handle {
            Ok(h) => {
                *slot = Some(h);
                Ok(())
            }
            Err(e) => {
                *self.disk.lock().unwrap_or_else(|e| e.into_inner()) =
                    disk_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                Err(crate::error::Error::Block(e.to_string()))
            }
        }
    }

    /// Stop the worker, restore the disk handle, and propagate worker errors.
    pub fn stop_worker(&self) -> crate::error::Result<()> {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.kick.write(1);
        let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(());
        };
        match handle.join() {
            Ok(outcome) => {
                *self.disk.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome.disk);
                outcome.result
            }
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

struct OpenedDisk {
    buffered_file: std::fs::File,
    direct_file: Option<std::fs::File>,
    readonly: bool,
    direct_align: u64,
}

fn open_disk_image(path: &std::path::Path) -> crate::error::Result<OpenedDisk> {
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| crate::error::Error::Block("disk path contains interior NUL".into()))?;

    let (buffered_file, readonly) = open_buffered(&path_c)?;
    let direct_align = logical_block_size(buffered_file.as_raw_fd());
    let direct_file = open_direct(&path_c, readonly);

    Ok(OpenedDisk {
        buffered_file,
        direct_file,
        readonly,
        direct_align,
    })
}

fn open_buffered(path_c: &std::ffi::CString) -> crate::error::Result<(std::fs::File, bool)> {
    // SAFETY: valid C string; open returns a new fd or -1.
    let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
    if fd >= 0 {
        // SAFETY: fd is owned.
        return Ok((unsafe { std::fs::File::from_raw_fd(fd) }, false));
    }
    let err = std::io::Error::last_os_error();
    let access_denied = matches!(err.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
        || err.kind() == std::io::ErrorKind::PermissionDenied;

    if !access_denied {
        return Err(crate::error::Error::ImageIo(err));
    }

    // SAFETY: RO open after permission failure on RDWR.
    let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC, 0) };
    if fd < 0 {
        return Err(crate::error::Error::ImageIo(std::io::Error::last_os_error()));
    }
    // SAFETY: fd is owned.
    Ok((unsafe { std::fs::File::from_raw_fd(fd) }, true))
}

fn open_direct(path_c: &std::ffi::CString, readonly: bool) -> Option<std::fs::File> {
    let flags = if readonly {
        libc::O_RDONLY | libc::O_DIRECT | libc::O_CLOEXEC
    } else {
        libc::O_RDWR | libc::O_DIRECT | libc::O_CLOEXEC
    };
    // SAFETY: valid C string.
    let fd = unsafe { libc::open(path_c.as_ptr(), flags, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is owned.
    Some(unsafe { std::fs::File::from_raw_fd(fd) })
}

fn logical_block_size(fd: libc::c_int) -> u64 {
    // SAFETY: fstat on a valid open fd.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd, &mut st) };
    if rc != 0 {
        return SECTOR_SIZE.max(IO_ALIGN as u64);
    }
    let bs = st.st_blksize as u64;
    if bs == 0 {
        SECTOR_SIZE.max(IO_ALIGN as u64)
    } else {
        bs.max(SECTOR_SIZE)
    }
}

fn worker_loop(
    transport: std::sync::Arc<std::sync::Mutex<BlockTransport>>,
    mut disk: BlockDisk,
    kick: vmm_sys_util::eventfd::EventFd,
    mem: vm_memory::GuestMemoryMmap<()>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> WorkerOutcome {
    let kick_fd = kick.as_raw_fd();
    let mut result = Ok(());

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
            result = Err(crate::error::Error::Block(format!("poll: {err}")));
            break;
        }

        if fds[0].revents == 0 {
            continue;
        }
        let _ = kick.read();

        if let Err(e) = process_queue(&transport, &mut disk, &mem) {
            eprintln!("kitsune: virtio-blk: {e}");
        }
    }

    WorkerOutcome { result, disk }
}

fn process_queue(
    transport: &std::sync::Mutex<BlockTransport>,
    disk: &mut BlockDisk,
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

        let written = handle_request(
            disk,
            capacity_sectors,
            readonly,
            driver_features,
            mem,
            chain,
        )
        .unwrap_or(1);

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
    disk: &mut BlockDisk,
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
            if end > capacity_sectors * SECTOR_SIZE {
                status = VIRTIO_BLK_S_IOERR;
                disk.zero_scratch(data_len).map_err(|_| BlockReqError::Io)?;
            } else if disk.read_at(offset, data_len).is_err() {
                let _ = disk.zero_scratch(data_len);
                status = VIRTIO_BLK_S_IOERR;
            }
            let data = disk
                .scratch
                .as_slice(data_len)
                .map_err(|_| BlockReqError::Io)?;
            writer.write_all(data).map_err(|_| BlockReqError::Io)?;
            writer.write_all(&[status]).map_err(|_| BlockReqError::Io)?;
            Ok((data_len + 1) as u32)
        }
        VIRTIO_BLK_T_OUT => {
            let data_len = reader.available_bytes();
            {
                let buf = disk
                    .scratch
                    .as_mut(data_len)
                    .map_err(|_| BlockReqError::Io)?;
                reader.read_exact(buf).map_err(|_| BlockReqError::Io)?;
            }
            let offset = hdr
                .sector
                .checked_mul(SECTOR_SIZE)
                .ok_or(BlockReqError::Io)?;
            let end = offset
                .checked_add(data_len as u64)
                .ok_or(BlockReqError::Io)?;

            let readonly = readonly || (driver_features & VIRTIO_BLK_F_RO) != 0;
            let out_of_range = end > capacity_sectors * SECTOR_SIZE;
            let io_err = !readonly && !out_of_range && disk.write_at(offset, data_len).is_err();
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
            } else if disk.flush_data().is_err() {
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
                disk.zero_scratch(total - 1)
                    .map_err(|_| BlockReqError::Io)?;
                let pad = disk
                    .scratch
                    .as_slice(total - 1)
                    .map_err(|_| BlockReqError::Io)?;
                let _ = writer.write_all(pad);
            }
            let _ = writer.write_all(&[VIRTIO_BLK_S_UNSUPP]);
            Ok(total as u32)
        }
    }
}

impl BlockDisk {
    fn zero_scratch(&mut self, len: usize) -> Result<(), String> {
        self.scratch.as_mut(len)?.fill(0);
        Ok(())
    }

    fn prefer_direct(&self, offset: u64, len: usize) -> bool {
        self.use_direct
            && self.direct_file.is_some()
            && is_direct_aligned(offset, len, self.direct_align)
    }

    fn io_fd(&self, prefer_direct: bool) -> libc::c_int {
        if prefer_direct && let Some(f) = self.direct_file.as_ref() {
            return f.as_raw_fd();
        }
        self.buffered_file.as_raw_fd()
    }

    /// Disable O_DIRECT for the rest of the run (e.g. unexpected EINVAL).
    fn disable_direct(&mut self, why: &str) {
        if self.use_direct {
            eprintln!("kitsune: virtio-blk: disabling O_DIRECT ({why})");
            self.use_direct = false;
        }
    }

    /// Drop io_uring after draining completions so scratch is not shared with in-flight I/O.
    fn disable_ring(&mut self, why: &str) {
        if let Some(mut ring) = self.ring.take() {
            drain_ring(&mut ring);
            eprintln!("kitsune: virtio-blk: disabling io_uring ({why})");
        }
    }

    /// Read `len` bytes at `offset` into the scratch buffer.
    fn read_at(&mut self, offset: u64, len: usize) -> std::io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.scratch.ensure(len).map_err(std::io::Error::other)?;
        let mut prefer = self.prefer_direct(offset, len);
        let ptr = self.scratch.ptr;
        // SAFETY: ensure() guarantees `len` bytes at ptr; exclusive on this thread.
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr, len) };

        loop {
            let fd = self.io_fd(prefer);
            match self.read_at_fd(fd, buf, offset) {
                Ok(()) => return Ok(()),
                Err(e) if prefer && e.raw_os_error() == Some(libc::EINVAL) && self.use_direct => {
                    self.disable_direct("read EINVAL on O_DIRECT fd");
                    prefer = false;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn read_at_fd(&mut self, fd: libc::c_int, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        let ring_err = if let Some(ring) = self.ring.as_mut() {
            match ring_read(ring, fd, buf, offset) {
                Ok(()) => return Ok(()),
                Err(e) => Some(e.to_string()),
            }
        } else {
            None
        };
        if let Some(msg) = ring_err {
            // Drop ring before any pread on the same buffer.
            self.disable_ring(&msg);
        }
        pread_all(fd, buf, offset)
    }

    /// Write `len` bytes from the scratch buffer to `offset`.
    fn write_at(&mut self, offset: u64, len: usize) -> std::io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > self.scratch.cap {
            return Err(std::io::Error::other("scratch buffer too small for write"));
        }
        let mut prefer = self.prefer_direct(offset, len);
        let ptr = self.scratch.ptr;
        // SAFETY: caller filled `len` bytes into scratch; exclusive on this thread.
        let buf = unsafe { std::slice::from_raw_parts(ptr, len) };

        loop {
            let fd = self.io_fd(prefer);
            match self.write_at_fd(fd, buf, offset) {
                Ok(()) => return Ok(()),
                Err(e) if prefer && e.raw_os_error() == Some(libc::EINVAL) && self.use_direct => {
                    self.disable_direct("write EINVAL on O_DIRECT fd");
                    prefer = false;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn write_at_fd(&mut self, fd: libc::c_int, buf: &[u8], offset: u64) -> std::io::Result<()> {
        let ring_err = if let Some(ring) = self.ring.as_mut() {
            match ring_write(ring, fd, buf, offset) {
                Ok(()) => return Ok(()),
                Err(e) => Some(e.to_string()),
            }
        } else {
            None
        };
        if let Some(msg) = ring_err {
            self.disable_ring(&msg);
        }
        pwrite_all(fd, buf, offset)
    }

    fn flush_data(&mut self) -> std::io::Result<()> {
        // Flush both fds when direct is in use so dirty buffered pages are not left behind.
        if self.use_direct
            && let Some(f) = self.direct_file.as_ref()
        {
            self.fsync_fd(f.as_raw_fd())?;
        }
        self.fsync_fd(self.buffered_file.as_raw_fd())
    }

    fn fsync_fd(&mut self, fd: libc::c_int) -> std::io::Result<()> {
        let ring_err = if let Some(ring) = self.ring.as_mut() {
            match ring_fsync(ring, fd) {
                Ok(()) => return Ok(()),
                Err(e) => Some(e.to_string()),
            }
        } else {
            None
        };
        if let Some(msg) = ring_err {
            self.disable_ring(&msg);
        }
        // SAFETY: fdatasync on a valid open file descriptor.
        let rc = unsafe { libc::fdatasync(fd) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn is_direct_aligned(offset: u64, len: usize, align: u64) -> bool {
    align != 0 && offset.is_multiple_of(align) && (len as u64).is_multiple_of(align)
}

fn to_off_t(offset: u64) -> std::io::Result<libc::off_t> {
    libc::off_t::try_from(offset).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file offset does not fit in off_t",
        )
    })
}

fn pread_all(fd: libc::c_int, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let off = to_off_t(offset + done as u64)?;
        // SAFETY: buf[done..] is valid for writing.
        let n = unsafe { libc::pread(fd, buf[done..].as_mut_ptr().cast(), buf.len() - done, off) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "pread short read",
            ));
        }
        done += n as usize;
    }
    Ok(())
}

fn pwrite_all(fd: libc::c_int, buf: &[u8], offset: u64) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let off = to_off_t(offset + done as u64)?;
        // SAFETY: buf[done..] is valid for reading.
        let n = unsafe { libc::pwrite(fd, buf[done..].as_ptr().cast(), buf.len() - done, off) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "pwrite short write",
            ));
        }
        done += n as usize;
    }
    Ok(())
}

fn drain_ring(ring: &mut io_uring::IoUring) {
    let _ = ring.submitter().submit();
    while ring.completion().next().is_some() {}
}

fn ring_read(
    ring: &mut io_uring::IoUring,
    fd: libc::c_int,
    buf: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let chunk = &mut buf[done..];
        let len = u32::try_from(chunk.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "read too large"))?;
        let entry = io_uring::opcode::Read::new(io_uring::types::Fd(fd), chunk.as_mut_ptr(), len)
            .offset(offset + done as u64)
            .build()
            .user_data(RING_USER_READ);
        // SAFETY: entry points at `chunk` which lives until completion is reaped.
        unsafe {
            ring.submission()
                .push(&entry)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        ring.submit_and_wait(1)?;
        let cqe = ring
            .completion()
            .next()
            .ok_or_else(|| std::io::Error::other("io_uring read: missing completion"))?;
        if cqe.user_data() != RING_USER_READ {
            return Err(std::io::Error::other("io_uring read: unexpected user_data"));
        }
        let res = cqe.result();
        if res < 0 {
            return Err(std::io::Error::from_raw_os_error(-res));
        }
        if res == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "io_uring read: EOF",
            ));
        }
        done += res as usize;
    }
    Ok(())
}

fn ring_write(
    ring: &mut io_uring::IoUring,
    fd: libc::c_int,
    buf: &[u8],
    offset: u64,
) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let chunk = &buf[done..];
        let len = u32::try_from(chunk.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "write too large")
        })?;
        let entry = io_uring::opcode::Write::new(io_uring::types::Fd(fd), chunk.as_ptr(), len)
            .offset(offset + done as u64)
            .build()
            .user_data(RING_USER_WRITE);
        // SAFETY: entry points at `chunk` which lives until completion is reaped.
        unsafe {
            ring.submission()
                .push(&entry)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        ring.submit_and_wait(1)?;
        let cqe = ring
            .completion()
            .next()
            .ok_or_else(|| std::io::Error::other("io_uring write: missing completion"))?;
        if cqe.user_data() != RING_USER_WRITE {
            return Err(std::io::Error::other(
                "io_uring write: unexpected user_data",
            ));
        }
        let res = cqe.result();
        if res < 0 {
            return Err(std::io::Error::from_raw_os_error(-res));
        }
        if res == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "io_uring write: zero",
            ));
        }
        done += res as usize;
    }
    Ok(())
}

fn ring_fsync(ring: &mut io_uring::IoUring, fd: libc::c_int) -> std::io::Result<()> {
    let entry = io_uring::opcode::Fsync::new(io_uring::types::Fd(fd))
        .flags(io_uring::types::FsyncFlags::DATASYNC)
        .build()
        .user_data(RING_USER_FSYNC);
    // SAFETY: Only the fd is used.
    unsafe {
        ring.submission()
            .push(&entry)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    ring.submit_and_wait(1)?;
    let cqe = ring
        .completion()
        .next()
        .ok_or_else(|| std::io::Error::other("io_uring fsync: missing completion"))?;
    if cqe.user_data() != RING_USER_FSYNC {
        return Err(std::io::Error::other(
            "io_uring fsync: unexpected user_data",
        ));
    }
    let res = cqe.result();
    if res < 0 {
        Err(std::io::Error::from_raw_os_error(-res))
    } else {
        Ok(())
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
    use std::os::fd::AsRawFd as _;

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

    #[test]
    fn direct_alignment_uses_host_block_size() {
        assert!(super::is_direct_aligned(0, 4096, 4096));
        assert!(!super::is_direct_aligned(0, 512, 4096));
        assert!(super::is_direct_aligned(0, 512, 512));
        assert!(!super::is_direct_aligned(1, 512, 512));
    }

    #[test]
    fn aligned_buf_grows() {
        let mut buf = super::AlignedBuf::new(4096).unwrap();
        let s = buf.as_mut(100).unwrap();
        s.fill(0xab);
        let s2 = buf.as_mut(16 * 1024).unwrap();
        assert_eq!(s2.len(), 16 * 1024);
        assert_eq!(s2[0], 0xab);
    }

    #[test]
    fn pread_pwrite_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("kitsune-blk-test-{}.img", std::process::id()));
        {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            f.set_len(4096).unwrap();
            let fd = f.as_raw_fd();
            let data = [0x5au8; 512];
            super::pwrite_all(fd, &data, 512).unwrap();
            let mut got = [0u8; 512];
            super::pread_all(fd, &mut got, 512).unwrap();
            assert_eq!(got, data);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn to_off_t_accepts_normal_offsets() {
        assert!(super::to_off_t(0).is_ok());
        assert!(super::to_off_t(1 << 30).is_ok());
    }

    #[test]
    fn io_uring_read_write_roundtrip() {
        let mut ring = match io_uring::IoUring::new(8) {
            Ok(r) => r,
            Err(_) => return,
        };
        let dir = std::env::temp_dir();
        let path = dir.join(format!("kitsune-blk-iouring-{}.img", std::process::id()));
        {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            f.set_len(4096).unwrap();
            let fd = f.as_raw_fd();
            let data = [0x3cu8; 1024];
            super::ring_write(&mut ring, fd, &data, 0).unwrap();
            let mut got = [0u8; 1024];
            super::ring_read(&mut ring, fd, &mut got, 0).unwrap();
            assert_eq!(got, data);
            super::ring_fsync(&mut ring, fd).unwrap();
        }
        let _ = std::fs::remove_file(&path);
    }
}
