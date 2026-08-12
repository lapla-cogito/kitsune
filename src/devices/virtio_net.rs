//! Virtio-mmio network device backed by a host TAP.

use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::fd::IntoRawFd as _;
use virtio_queue::QueueT as _;

/// MMIO base for the virtio-net device (after virtio-blk window).
pub const VIRTIO_NET_MMIO_BASE: u64 = crate::memory::GUEST_RAM_END + 0x1000;
/// GSI / IRQ line for virtio-net.
pub const VIRTIO_NET_IRQ: u32 = 6;

const VIRTIO_ID_NET: u32 = 1;

// Virtio-net feature bits (linux/virtio_net.h).
const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
const VIRTIO_NET_F_GUEST_CSUM: u64 = 1 << 1;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_GUEST_TSO4: u64 = 1 << 7;
const VIRTIO_NET_F_GUEST_TSO6: u64 = 1 << 8;
const VIRTIO_NET_F_GUEST_ECN: u64 = 1 << 9;
const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
const VIRTIO_NET_F_HOST_TSO6: u64 = 1 << 12;
const VIRTIO_NET_F_HOST_ECN: u64 = 1 << 13;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;

/// Features always offered (identity + link status).
const BASE_FEATURES: u64 =
    super::virtio_mmio::VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS;

/// Offload features paired with TAP `TUNSETOFFLOAD`.
const OFFLOAD_FEATURES: u64 = VIRTIO_NET_F_CSUM
    | VIRTIO_NET_F_GUEST_CSUM
    | VIRTIO_NET_F_HOST_TSO4
    | VIRTIO_NET_F_HOST_TSO6
    | VIRTIO_NET_F_HOST_ECN
    | VIRTIO_NET_F_GUEST_TSO4
    | VIRTIO_NET_F_GUEST_TSO6
    | VIRTIO_NET_F_GUEST_ECN;

const VIRTIO_NET_S_LINK_UP: u16 = 1;

const QUEUE_RX: u32 = 0;
const QUEUE_TX: u32 = 1;
const QUEUE_MAX_SIZE: u16 = 256;

/// Linux uses `sizeof(virtio_net_hdr_mrg_rxbuf)` (= 12) whenever
/// `VIRTIO_F_VERSION_1` is negotiated, even without `MRG_RXBUF`.
const NET_HDR_LEN: usize = 12;
/// Max GSO payload (64 KiB) + Ethernet (+ VLAN) + virtio_net_hdr.
const MAX_FRAME: usize = 65536 + 18 + NET_HDR_LEN;

// TAP flags (linux/if_tun.h)
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;
const IFF_VNET_HDR: libc::c_short = 0x4000;

// TUNSETOFFLOAD feature bits
const TUN_F_CSUM: libc::c_uint = 0x01;
const TUN_F_TSO4: libc::c_uint = 0x02;
const TUN_F_TSO6: libc::c_uint = 0x04;
const TUN_F_TSO_ECN: libc::c_uint = 0x08;
const TUN_OFFLOADS: libc::c_uint = TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_TSO_ECN;

const CONFIG_SIZE: usize = 8; // mac[6] + status[2]

/// TX descriptor held until TAP accepts the full frame (backpressure).
struct PendingTx {
    head: u16,
    len: usize,
}

/// Mutable device state shared between MMIO handlers and the net worker.
struct NetState {
    mmio: super::virtio_mmio::VirtioMmio,
    tap: std::fs::File,
    mac: [u8; 6],
    rx_scratch: Vec<u8>,
    tx_scratch: Vec<u8>,
    /// Guest TX frame in `tx_scratch` waiting for TAP write space.
    pending_tx: Option<PendingTx>,
    /// Host RX frame length in `rx_scratch` waiting for a guest RX buffer.
    pending_rx_len: Option<usize>,
}

/// Virtio-mmio network device using a host TAP interface.
pub struct VirtioNet {
    base: u64,
    state: std::sync::Arc<std::sync::Mutex<NetState>>,
    /// Clone of the transport notify EventFd (ioeventfd + software kick / stop).
    kick: vmm_sys_util::eventfd::EventFd,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<crate::error::Result<()>>>>,
}

impl VirtioNet {
    pub const MMIO_BASE: u64 = VIRTIO_NET_MMIO_BASE;
    pub const IRQ: u32 = VIRTIO_NET_IRQ;

    /// Open or create TAP `ifname` and register IRQ `VIRTIO_NET_IRQ` with KVM.
    pub fn new(ifname: &str, vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        let (tap, offloads) = open_tap(ifname)?;
        let mac = mac_from_name(ifname);
        let features = advertised_features(offloads);

        let mmio = super::virtio_mmio::VirtioMmio::new(
            VIRTIO_NET_MMIO_BASE,
            VIRTIO_ID_NET,
            features,
            2,
            QUEUE_MAX_SIZE,
        )
        .map_err(crate::error::Error::Net)?;
        mmio.register_irq(vm, VIRTIO_NET_IRQ)
            .map_err(crate::error::Error::KvmIoctl)?;
        mmio.register_ioeventfds(vm)
            .map_err(crate::error::Error::KvmIoctl)?;

        // Same EventFd KVM signals on QueueNotify (ioeventfd) and we use for stop.
        let kick = mmio
            .notify_fd()
            .try_clone()
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;

        Ok(Self {
            base: VIRTIO_NET_MMIO_BASE,
            state: std::sync::Arc::new(std::sync::Mutex::new(NetState {
                mmio,
                tap,
                mac,
                rx_scratch: vec![0u8; MAX_FRAME],
                tx_scratch: Vec::with_capacity(MAX_FRAME),
                pending_tx: None,
                pending_rx_len: None,
            })),
            kick,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker: std::sync::Mutex::new(None),
        })
    }

    /// Start the TAP/queue worker.
    pub fn start_worker(&self, mem: vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        let mut slot = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Ok(());
        }

        self.stop.store(false, std::sync::atomic::Ordering::SeqCst);
        let state = std::sync::Arc::clone(&self.state);
        let kick = self
            .kick
            .try_clone()
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        let stop = std::sync::Arc::clone(&self.stop);

        let handle = std::thread::Builder::new()
            .name("virtio-net".into())
            .spawn(move || worker_loop(state, kick, mem, stop))
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
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
            Err(_) => Err(crate::error::Error::Net(
                "virtio-net worker panicked".into(),
            )),
        }
    }

    pub fn handles(&self, addr: u64) -> bool {
        (self.base..self.base + super::virtio_mmio::MMIO_SIZE).contains(&addr)
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = config_space(&state.mac);
        state.mmio.read(addr, data, &cfg);
    }

    pub fn write(
        &self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        let notify = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .mmio
                .write(addr, data, mem)
                .map_err(crate::error::Error::Net)?
        };
        if notify.is_some() {
            self.kick
                .write(1)
                .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        }
        Ok(())
    }
}

impl Drop for VirtioNet {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

fn config_space(mac: &[u8; 6]) -> [u8; CONFIG_SIZE] {
    let mut cfg = [0u8; CONFIG_SIZE];
    cfg[..6].copy_from_slice(mac);
    cfg[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
    cfg
}

fn worker_loop(
    state: std::sync::Arc<std::sync::Mutex<NetState>>,
    kick: vmm_sys_util::eventfd::EventFd,
    mem: vm_memory::GuestMemoryMmap<()>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> crate::error::Result<()> {
    let tap_fd = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tap
        .as_raw_fd();
    let kick_fd = kick.as_raw_fd();

    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let (want_tap_in, want_tap_out) = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            // Hold off TAP reads while an RX frame waits for guest buffers.
            let want_in = s.pending_rx_len.is_none();
            // Only wait for writable TAP when we can actually complete TX.
            let tx_ready = s
                .mmio
                .queue(QUEUE_TX)
                .is_some_and(virtio_queue::QueueT::ready);
            let want_out = s.pending_tx.is_some() && tx_ready;
            (want_in, want_out)
        };

        let mut tap_events = 0;
        if want_tap_in {
            tap_events |= libc::POLLIN;
        }
        if want_tap_out {
            tap_events |= libc::POLLOUT;
        }

        let mut fds = [
            libc::pollfd {
                fd: if tap_events != 0 { tap_fd } else { -1 },
                events: tap_events,
                revents: 0,
            },
            libc::pollfd {
                fd: kick_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll on valid fds owned for the worker lifetime.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 50) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(crate::error::Error::Net(format!("poll: {err}")));
        }

        let kick_ready = fds[1].revents != 0;
        if kick_ready {
            // Clear kick; coalesced notifies are fine.
            let _ = kick.read();
        }
        let tap_ready = fds[0].fd >= 0 && fds[0].revents != 0;
        if !kick_ready && !tap_ready {
            continue;
        }

        // Datapath errors are logged and retried; only poll failures are fatal.
        if let Err(e) = process_net_once(&state, &mem) {
            eprintln!("kitsune: virtio-net: {e}");
        }
    }
    Ok(())
}

fn process_net_once(
    state: &std::sync::Arc<std::sync::Mutex<NetState>>,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<()> {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let mut need_irq = false;
    let mut first_err = None;

    // Always raise used-queue IRQ if any descriptor was completed, even when a
    // later step returns an error (otherwise the guest may miss add_used).
    match process_tx(&mut guard, mem) {
        Ok(used) => need_irq |= used,
        Err((used, e)) => {
            need_irq |= used;
            first_err = Some(e);
        }
    }
    match process_rx(&mut guard, mem) {
        Ok(used) => need_irq |= used,
        Err((used, e)) => {
            need_irq |= used;
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }

    if need_irq {
        guard
            .mmio
            .signal_used_queue()
            .map_err(crate::error::Error::Net)?;
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Result of attempting to send one frame to the TAP.
#[derive(Debug, PartialEq, Eq)]
enum TapSend {
    Sent,
    WouldBlock,
}

/// Write one full TAP frame. Does not drop on backpressure.
fn tap_try_send(tap: &mut std::fs::File, packet: &[u8]) -> crate::error::Result<TapSend> {
    loop {
        match tap.write(packet) {
            Ok(n) if n == packet.len() => return Ok(TapSend::Sent),
            // TAP is datagram-like; a short write is not recoverable as a partial frame.
            Ok(n) => {
                return Err(crate::error::Error::Net(format!(
                    "tap short write: {n}/{}",
                    packet.len()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(TapSend::WouldBlock);
            }
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => return Err(crate::error::Error::Net(format!("tap write: {e}"))),
        }
    }
}

fn complete_tx(
    state: &mut NetState,
    mem: &vm_memory::GuestMemoryMmap<()>,
    head: u16,
) -> crate::error::Result<()> {
    state
        .mmio
        .queue_mut(QUEUE_TX)
        .ok_or_else(|| crate::error::Error::Net("missing tx queue".into()))?
        .add_used(mem, head, 0)
        .map_err(|e| crate::error::Error::Net(e.to_string()))
}

/// TX/RX step result: `Ok(used)` or `Err((used, error))` so callers can signal
/// the used ring even when a later hard error occurs.
type StepResult = std::result::Result<bool, (bool, crate::error::Error)>;

fn process_tx(state: &mut NetState, mem: &vm_memory::GuestMemoryMmap<()>) -> StepResult {
    let tx_ready = state
        .mmio
        .queue(QUEUE_TX)
        .is_some_and(virtio_queue::QueueT::ready);
    if !tx_ready {
        // Drop held TX across reset / queue not-ready so head indices cannot
        // leak into a re-initialized ring.
        state.pending_tx = None;
        return Ok(false);
    }

    let mut used_any = false;

    // Flush a frame held from a previous EAGAIN before taking more descriptors.
    if let Some(pending) = state.pending_tx.take() {
        let packet = &state.tx_scratch[..pending.len];
        match tap_try_send(&mut state.tap, packet) {
            Ok(TapSend::Sent) => {
                complete_tx(state, mem, pending.head).map_err(|e| (used_any, e))?;
                used_any = true;
            }
            Ok(TapSend::WouldBlock) => {
                state.pending_tx = Some(pending);
                return Ok(used_any);
            }
            Err(e) => {
                let _ = complete_tx(state, mem, pending.head);
                return Err((true, e));
            }
        }
    }

    loop {
        let chain = {
            let Some(q) = state.mmio.queue_mut(QUEUE_TX) else {
                break;
            };
            match q.pop_descriptor_chain(mem) {
                Some(c) => c,
                None => break,
            }
        };
        let head = chain.head_index();
        let mut reader = match virtio_queue::Reader::new(mem, chain) {
            Ok(r) => r,
            Err(e) => {
                let _ = complete_tx(state, mem, head);
                return Err((true, crate::error::Error::Net(e.to_string())));
            }
        };
        let total = reader.available_bytes();

        // Malformed / empty: complete immediately (nothing to send).
        if total < NET_HDR_LEN {
            complete_tx(state, mem, head).map_err(|e| (used_any, e))?;
            used_any = true;
            continue;
        }

        if state.tx_scratch.len() < total {
            state.tx_scratch.resize(total, 0);
        }
        if let Err(e) = reader.read_exact(&mut state.tx_scratch[..total]) {
            let _ = complete_tx(state, mem, head);
            return Err((true, crate::error::Error::Net(e.to_string())));
        }

        match tap_try_send(&mut state.tap, &state.tx_scratch[..total]) {
            Ok(TapSend::Sent) => {
                complete_tx(state, mem, head).map_err(|e| (used_any, e))?;
                used_any = true;
            }
            Ok(TapSend::WouldBlock) => {
                state.pending_tx = Some(PendingTx { head, len: total });
                break;
            }
            Err(e) => {
                let _ = complete_tx(state, mem, head);
                return Err((true, e));
            }
        }
    }
    Ok(used_any)
}

fn process_rx(state: &mut NetState, mem: &vm_memory::GuestMemoryMmap<()>) -> StepResult {
    let rx_ready = state
        .mmio
        .queue(QUEUE_RX)
        .is_some_and(virtio_queue::QueueT::ready);
    if !rx_ready {
        // Drop held RX across reset / queue not-ready.
        state.pending_rx_len = None;
        return Ok(false);
    }

    let mut used_any = false;

    // Deliver a held frame before reading more from the TAP.
    if state.pending_rx_len.is_some() {
        used_any |= flush_pending_rx(state, mem).map_err(|e| (used_any, e))?;
        if state.pending_rx_len.is_some() {
            // Still blocked on guest RX buffers; do not read more.
            return Ok(used_any);
        }
    }

    loop {
        match state.tap.read(&mut state.rx_scratch) {
            Ok(0) => break,
            Ok(n) if n < NET_HDR_LEN => {
                // Undersized TAP read: discard (not a valid virtio-net frame).
                continue;
            }
            Ok(n) => {
                state.pending_rx_len = Some(n);
                used_any |= flush_pending_rx(state, mem).map_err(|e| (used_any, e))?;
                if state.pending_rx_len.is_some() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => {
                return Err((used_any, crate::error::Error::Net(format!("tap read: {e}"))));
            }
        }
    }
    Ok(used_any)
}

/// Try to place `pending_rx_len` into guest RX buffers.
///
/// Undersized guest chains are returned with used length 0 so the driver can
/// recycle them. If the avail ring is exhausted without a large enough buffer,
/// the pending frame is dropped so TAP `POLLIN` can resume.
fn flush_pending_rx(
    state: &mut NetState,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<bool> {
    let Some(frame_len) = state.pending_rx_len else {
        return Ok(false);
    };
    let mut used_any = false;
    let mut saw_chain = false;
    let mut saw_fit = false;

    loop {
        let chain = {
            let Some(q) = state.mmio.queue_mut(QUEUE_RX) else {
                break;
            };
            match q.pop_descriptor_chain(mem) {
                Some(c) => c,
                None => break,
            }
        };
        saw_chain = true;
        let head = chain.head_index();
        let mut writer = virtio_queue::Writer::new(mem, chain)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;

        if writer.available_bytes() < frame_len {
            // Buffer too small: give it back empty and try the next chain.
            state
                .mmio
                .queue_mut(QUEUE_RX)
                .ok_or_else(|| crate::error::Error::Net("missing rx queue".into()))?
                .add_used(mem, head, 0)
                .map_err(|e| crate::error::Error::Net(e.to_string()))?;
            used_any = true;
            continue;
        }

        let frame = &state.rx_scratch[..frame_len];
        writer
            .write_all(frame)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        state
            .mmio
            .queue_mut(QUEUE_RX)
            .ok_or_else(|| crate::error::Error::Net("missing rx queue".into()))?
            .add_used(mem, head, frame_len as u32)
            .map_err(|e| crate::error::Error::Net(e.to_string()))?;
        state.pending_rx_len = None;
        used_any = true;
        saw_fit = true;
        break;
    }

    // Avail ring drained without a large enough buffer: drop the frame so the
    // device can resume TAP reads (avoids permanent POLLIN starve).
    if state.pending_rx_len.is_some() && saw_chain && !saw_fit {
        eprintln!(
            "kitsune: virtio-net: dropping {frame_len}-byte RX frame (no guest buffer large enough)"
        );
        state.pending_rx_len = None;
    }

    Ok(used_any)
}

/// Minimal `struct ifreq` layout for `TUNSETIFF` on Linux.
#[repr(C)]
struct IfReq {
    name: [libc::c_char; 16],
    flags: libc::c_short,
    _pad: [u8; 22],
}

/// Open TAP and enable offloads when the kernel supports them.
/// Returns `(file, offloads_enabled)`.
fn open_tap(ifname: &str) -> crate::error::Result<(std::fs::File, bool)> {
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
        flags: IFF_TAP | IFF_NO_PI | IFF_VNET_HDR,
        _pad: [0; 22],
    };
    for (i, b) in ifname.bytes().enumerate() {
        ifr.name[i] = b as libc::c_char;
    }

    const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
    const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;
    const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;

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

    // SAFETY: enable checksum / TSO offloads on the TAP.
    let offloads = TUN_OFFLOADS;
    let rc = unsafe { libc::ioctl(owned.as_raw_fd(), TUNSETOFFLOAD, offloads) };
    let offloads_ok = rc >= 0;

    // SAFETY: OwnedFd is consumed into a File.
    let file = unsafe { std::fs::File::from_raw_fd(owned.into_raw_fd()) };
    Ok((file, offloads_ok))
}

fn advertised_features(offloads: bool) -> u64 {
    if offloads {
        BASE_FEATURES | OFFLOAD_FEATURES
    } else {
        BASE_FEATURES
    }
}

fn mac_from_name(ifname: &str) -> [u8; 6] {
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

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::io::Write as _;
    use std::os::fd::FromRawFd as _;

    #[test]
    fn features_include_offloads_when_enabled() {
        let base = super::advertised_features(false);
        assert_eq!(base & super::OFFLOAD_FEATURES, 0);
        assert_ne!(base & super::VIRTIO_NET_F_MAC, 0);
        assert_ne!(base & (1u64 << 32), 0);

        let full = super::advertised_features(true);
        assert_ne!(full & super::VIRTIO_NET_F_CSUM, 0);
        assert_ne!(full & super::VIRTIO_NET_F_GUEST_CSUM, 0);
        assert_ne!(full & super::VIRTIO_NET_F_HOST_TSO4, 0);
        assert_ne!(full & super::VIRTIO_NET_F_HOST_TSO6, 0);
        assert_ne!(full & super::VIRTIO_NET_F_GUEST_TSO4, 0);
        assert_ne!(full & super::VIRTIO_NET_F_GUEST_TSO6, 0);
        assert_ne!(full & super::VIRTIO_NET_F_HOST_ECN, 0);
        assert_ne!(full & super::VIRTIO_NET_F_GUEST_ECN, 0);
        assert_eq!(full & (1 << 10), 0);
        assert_eq!(full & (1 << 14), 0);
        assert_eq!(full & (1 << 15), 0);
    }

    #[test]
    fn max_frame_fits_gso() {
        const { assert!(super::MAX_FRAME >= 65536 + super::NET_HDR_LEN) };
    }

    fn nonblock_pipe() -> (std::fs::File, std::fs::File) {
        let mut fds = [0; 2];
        // SAFETY: pipe2 with valid stack array.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        assert_eq!(rc, 0, "pipe2 failed");
        // SAFETY: exclusive ownership of the two fds.
        let r = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let w = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        (r, w)
    }

    #[test]
    fn tap_try_send_would_block_does_not_claim_sent() {
        let (mut reader, mut writer) = nonblock_pipe();
        // Fill the pipe so the next write blocks.
        let chunk = vec![0xabu8; 4096];
        loop {
            match writer.write(&chunk) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("fill pipe: {e}"),
            }
        }
        let packet = vec![0xcd; 2048];
        match super::tap_try_send(&mut writer, &packet) {
            Ok(super::TapSend::WouldBlock) => {}
            other => panic!("expected WouldBlock, got {other:?}"),
        }
        // Drain so Drop does not hang on some kernels.
        let mut buf = [0u8; 4096];
        while reader.read(&mut buf).unwrap_or(0) > 0 {}
    }

    #[test]
    fn tap_try_send_full_packet() {
        let (mut reader, mut writer) = nonblock_pipe();
        let packet = b"virtio-net-frame\0\0\0\0";
        assert_eq!(
            super::tap_try_send(&mut writer, packet).unwrap(),
            super::TapSend::Sent
        );
        let mut got = vec![0u8; packet.len()];
        assert_eq!(reader.read(&mut got).unwrap(), packet.len());
        assert_eq!(&got[..], packet);
    }
}
