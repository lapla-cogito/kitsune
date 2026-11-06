//! Virtio-mmio vsock device. Stream sockets only.
//!
//! The host API is the Unix socket owned by `super::vsock_mux`.

use std::io::Read as _;
use std::io::Write as _;
use virtio_queue::QueueT as _;

/// MMIO base for virtio-vsock (after the virtio-net window).
pub const VIRTIO_VSOCK_MMIO_BASE: u64 = crate::memory::GUEST_RAM_END + 0x2000;
/// GSI / IRQ line for virtio-vsock.
pub const VIRTIO_VSOCK_IRQ: u32 = 7;

const VIRTIO_ID_VSOCK: u32 = 19;
const VIRTIO_VSOCK_F_STREAM: u64 = 1 << 0;
const QUEUE_RX: u32 = 0;
const QUEUE_TX: u32 = 1;
const QUEUE_EVENT: u32 = 2;
const QUEUE_MAX_SIZE: u16 = 256;
const EVENT_TRANSPORT_RESET: u32 = 0;

struct VsockState {
    mmio: super::virtio_mmio::VirtioMmio,
    mux: super::vsock_mux::Mux,
    queues_ready: bool,
    need_reset_event: bool,
}

impl super::virtio_dev::VirtioState for VsockState {
    fn mmio(&self) -> &super::virtio_mmio::VirtioMmio {
        &self.mmio
    }

    fn mmio_mut(&mut self) -> &mut super::virtio_mmio::VirtioMmio {
        &mut self.mmio
    }

    fn copy_config(&self, dest: &mut [u8]) -> usize {
        let bytes = self.mux.guest_cid().to_le_bytes();
        let n = bytes.len().min(dest.len());
        dest[..n].copy_from_slice(&bytes[..n]);
        bytes.len()
    }
}

pub struct VirtioVsock {
    dev: super::virtio_dev::VirtioDev<VsockState, crate::error::Result<()>>,
}

impl VirtioVsock {
    pub const MMIO_BASE: u64 = VIRTIO_VSOCK_MMIO_BASE;
    pub const IRQ: u32 = VIRTIO_VSOCK_IRQ;

    pub fn new(
        guest_cid: u64,
        uds_path: &std::path::Path,
        vm: &kvm_ioctls::VmFd,
    ) -> crate::error::Result<Self> {
        let mux = super::vsock_mux::Mux::bind(guest_cid, uds_path)?;
        let mmio = super::virtio_mmio::VirtioMmio::new(
            VIRTIO_VSOCK_MMIO_BASE,
            VIRTIO_ID_VSOCK,
            super::virtio_mmio::VIRTIO_F_VERSION_1 | VIRTIO_VSOCK_F_STREAM,
            3,
            QUEUE_MAX_SIZE,
        )?;
        mmio.register_irq(vm, VIRTIO_VSOCK_IRQ)
            .map_err(crate::error::Error::KvmIoctl)?;
        mmio.register_ioeventfds(vm)
            .map_err(crate::error::Error::KvmIoctl)?;
        let dev = super::virtio_dev::VirtioDev::new(
            VIRTIO_VSOCK_MMIO_BASE,
            VsockState {
                mmio,
                mux,
                queues_ready: false,
                need_reset_event: false,
            },
        )?;
        Ok(Self { dev })
    }

    pub fn start_worker(&self, mem: vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        let Some(reservation) = self.dev.reserve() else {
            return Ok(());
        };
        let state = self.dev.state();
        let kick = self
            .dev
            .clone_kick()
            .map_err(|err| crate::error::Error::Vsock(err.to_string()))?;
        let stop = self.dev.stop_flag();
        self.dev.clear_stop();
        let handle = std::thread::Builder::new()
            .name("virtio-vsock".into())
            .spawn(move || worker_loop(state, kick, mem, stop))
            .map_err(|err| crate::error::Error::Vsock(err.to_string()))?;
        reservation.commit(handle);
        Ok(())
    }

    pub fn stop_worker(&self) -> crate::error::Result<()> {
        self.dev.request_stop();
        let Some(handle) = self.dev.take_worker() else {
            return Ok(());
        };
        match handle.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(crate::error::Error::Vsock(
                "virtio-vsock worker panicked".into(),
            )),
        }
    }

    pub fn handles(&self, addr: u64) -> bool {
        self.dev.handles(addr)
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        self.dev.read(addr, data);
    }

    pub fn write(
        &self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        self.dev.write(addr, data, mem)
    }
}

impl super::mmio_device::MmioDevice for VirtioVsock {
    fn kind(&self) -> super::mmio_device::MmioKind {
        super::mmio_device::MmioKind::Vsock
    }

    fn range(&self) -> std::ops::Range<u64> {
        self.dev.range()
    }

    fn acpi(&self) -> crate::acpi::VirtioMmioAcpi {
        crate::acpi::VirtioMmioAcpi {
            base: Self::MMIO_BASE as u32,
            size: super::virtio_mmio::MMIO_SIZE as u32,
            irq: Self::IRQ,
            uid: 2,
        }
    }

    fn read(&self, addr: u64, data: &mut [u8]) {
        VirtioVsock::read(self, addr, data);
    }

    fn write(
        &self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        VirtioVsock::write(self, addr, data, mem)
    }

    fn start(&self, mem: vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        self.start_worker(mem)
    }

    fn stop(&self) -> crate::error::Result<()> {
        self.stop_worker()
    }
}

fn transport_up(state: &VsockState) -> bool {
    let ready = |index: u32| {
        state
            .mmio
            .queue(index)
            .is_some_and(virtio_queue::QueueT::ready)
    };
    ready(QUEUE_RX) && ready(QUEUE_TX)
}

fn worker_loop(
    state: std::sync::Arc<std::sync::Mutex<VsockState>>,
    kick: vmm_sys_util::eventfd::EventFd,
    mem: vm_memory::GuestMemoryMmap<()>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> crate::error::Result<()> {
    use std::os::fd::AsRawFd as _;

    let kick_fd = kick.as_raw_fd();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let interests = {
            let guard = state.lock().unwrap_or_else(|err| err.into_inner());
            guard.mux.interests()
        };
        let mut fds = Vec::with_capacity(interests.len() + 1);
        fds.push(libc::pollfd {
            fd: kick_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        for (fd, events) in interests {
            fds.push(libc::pollfd {
                fd,
                events,
                revents: 0,
            });
        }
        // SAFETY: poll on the kick eventfd and sockets owned by the mux.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 50) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(crate::error::Error::Vsock(format!("poll: {err}")));
        }
        let mut guard = state.lock().unwrap_or_else(|err| err.into_inner());
        if fds[0].revents != 0 {
            let _ = kick.read();
        }
        for pfd in fds.iter().skip(1) {
            if pfd.revents != 0 {
                guard.mux.on_fd(pfd.fd, pfd.revents);
            }
        }
        guard.mux.check_timeouts(std::time::Instant::now());
        if let Err(err) = drive(&mut guard, &mem) {
            eprintln!("kitsune: virtio-vsock: {err}");
        }
    }
    Ok(())
}

fn drive(state: &mut VsockState, mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
    let up = transport_up(state);
    if state.queues_ready && !up {
        state.queues_ready = false;
        state.need_reset_event = true;
        state.mux.kill_connections();
    } else if up && !state.queues_ready {
        state.queues_ready = true;
    }
    if !up {
        return Ok(());
    }

    let mut used = pull_tx(state, mem)?;
    if push_rx(state, mem)? {
        used = true;
    }
    if state.need_reset_event && push_reset_event(state, mem)? {
        state.need_reset_event = false;
        used = true;
    }
    if used {
        state.mmio.signal_used_queue()?;
    }
    Ok(())
}

fn pull_tx(
    state: &mut VsockState,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<bool> {
    let mut used = false;
    loop {
        let chain = {
            let Some(queue) = state.mmio.queue_mut(QUEUE_TX) else {
                break;
            };
            if !queue.ready() {
                break;
            }
            queue.pop_descriptor_chain(mem)
        };
        let Some(chain) = chain else {
            break;
        };
        let head = chain.head_index();
        let mut reader = virtio_queue::Reader::new(mem, chain)
            .map_err(|err| crate::error::Error::Vsock(format!("tx chain: {err}")))?;
        let mut raw = [0u8; super::vsock_mux::HDR_LEN];
        if reader.read_exact(&mut raw).is_err() {
            add_used(state, QUEUE_TX, mem, head, 0)?;
            used = true;
            continue;
        }
        let hdr = crate::devices::vsock_mux::Header::parse(&raw);
        let len = hdr.len as usize;
        if len > 64 * 1024 {
            state.mux.reset_flow(hdr.src_port, hdr.dst_port);
            add_used(state, QUEUE_TX, mem, head, 0)?;
            used = true;
            continue;
        }
        let mut data = vec![0u8; len];
        if len > 0 && reader.read_exact(&mut data).is_err() {
            state.mux.reset_flow(hdr.src_port, hdr.dst_port);
            add_used(state, QUEUE_TX, mem, head, 0)?;
            used = true;
            continue;
        }
        state.mux.handle_guest_pkt(&hdr, &data);
        add_used(
            state,
            QUEUE_TX,
            mem,
            head,
            (super::vsock_mux::HDR_LEN + len) as u32,
        )?;
        used = true;
    }
    Ok(used)
}

fn push_rx(
    state: &mut VsockState,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<bool> {
    let mut used = false;
    loop {
        if !state.mux.has_out() {
            break;
        }
        let chain = {
            let Some(queue) = state.mmio.queue_mut(QUEUE_RX) else {
                break;
            };
            if !queue.ready() {
                break;
            }
            queue.pop_descriptor_chain(mem)
        };
        let Some(chain) = chain else {
            break;
        };
        let head = chain.head_index();
        let mut writer = virtio_queue::Writer::new(mem, chain)
            .map_err(|err| crate::error::Error::Vsock(format!("rx chain: {err}")))?;
        let avail = writer.available_bytes();
        let Some(mut pkt) = state.mux.pop_out() else {
            add_used(state, QUEUE_RX, mem, head, 0)?;
            used = true;
            break;
        };
        if avail < super::vsock_mux::HDR_LEN {
            state.mux.push_front_out(pkt);
            add_used(state, QUEUE_RX, mem, head, 0)?;
            used = true;
            break;
        }
        let space = avail - super::vsock_mux::HDR_LEN;
        if pkt.op != super::vsock_mux::OP_RW && space < pkt.data.len() {
            state.mux.push_front_out(pkt);
            add_used(state, QUEUE_RX, mem, head, 0)?;
            used = true;
            break;
        }
        let n = pkt.data.len().min(space);
        let rest = if n < pkt.data.len() {
            Some(pkt.data.split_off(n))
        } else {
            None
        };
        let (buf_alloc, fwd_cnt) = state.mux.credit_of(pkt.flow);
        let raw = pkt.header(state.mux.guest_cid(), buf_alloc, fwd_cnt);
        if writer.write_all(&raw).is_err() || (n > 0 && writer.write_all(&pkt.data).is_err()) {
            state.mux.push_front_out(pkt);
            add_used(state, QUEUE_RX, mem, head, 0)?;
            used = true;
            break;
        }
        if let Some(rest) = rest {
            state.mux.push_front_out(super::vsock_mux::OutPkt {
                flow: pkt.flow,
                op: pkt.op,
                flags: pkt.flags,
                data: rest,
            });
        }
        add_used(
            state,
            QUEUE_RX,
            mem,
            head,
            (super::vsock_mux::HDR_LEN + n) as u32,
        )?;
        used = true;
    }
    Ok(used)
}

fn push_reset_event(
    state: &mut VsockState,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<bool> {
    let chain = {
        let Some(queue) = state.mmio.queue_mut(QUEUE_EVENT) else {
            return Ok(false);
        };
        if !queue.ready() {
            return Ok(false);
        }
        queue.pop_descriptor_chain(mem)
    };
    let Some(chain) = chain else {
        return Ok(false);
    };
    let head = chain.head_index();
    let mut writer = virtio_queue::Writer::new(mem, chain)
        .map_err(|err| crate::error::Error::Vsock(format!("event chain: {err}")))?;
    if writer.available_bytes() < 4 {
        add_used(state, QUEUE_EVENT, mem, head, 0)?;
        return Ok(true);
    }
    writer
        .write_all(&EVENT_TRANSPORT_RESET.to_le_bytes())
        .map_err(|err| crate::error::Error::Vsock(format!("event write: {err}")))?;
    add_used(state, QUEUE_EVENT, mem, head, 4)?;
    Ok(true)
}

fn add_used(
    state: &mut VsockState,
    queue: u32,
    mem: &vm_memory::GuestMemoryMmap<()>,
    head: u16,
    len: u32,
) -> crate::error::Result<()> {
    state
        .mmio
        .queue_mut(queue)
        .ok_or_else(|| crate::error::Error::Vsock("missing virtqueue".into()))?
        .add_used(mem, head, len)
        .map_err(|err| crate::error::Error::Vsock(err.to_string()))
}
