//! Host side of virtio-vsock.

use std::io::ErrorKind;
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileTypeExt as _;

const HOST_CID: u64 = 2;
const TYPE_STREAM: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
pub(crate) const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;
const SHUTDOWN_RCV: u32 = 1;
const SHUTDOWN_SEND: u32 = 2;

pub(crate) const HDR_LEN: usize = 44;
const BUF_ALLOC: u32 = 256 * 1024;
const MAX_CHUNK: usize = 16 * 1024;
const MAX_PAYLOAD: usize = 64 * 1024;
const MAX_CONNS: usize = 128;
const MAX_PREFETCH: usize = 64 * 1024;
/// `sun_path` holds 108 bytes including the trailing NUL. `_` + a u32 port is 11.
const MAX_UDS_PATH: usize = 107 - 11;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn validate_guest_cid(cid: u64) -> crate::error::Result<()> {
    if (3..0xffff_ffff).contains(&cid) {
        Ok(())
    } else {
        Err(crate::error::Error::Vsock(format!(
            "guest CID {cid} is reserved or does not fit in 32 bits"
        )))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub type_: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl Header {
    pub(crate) fn parse(raw: &[u8; HDR_LEN]) -> Self {
        let u64_at = |off: usize| u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
        let u32_at = |off: usize| u32::from_le_bytes(raw[off..off + 4].try_into().unwrap());
        let u16_at = |off: usize| u16::from_le_bytes(raw[off..off + 2].try_into().unwrap());
        Self {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            type_: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Flow {
    pub guest_port: u32,
    pub host_port: u32,
}

#[derive(Debug)]
pub(crate) struct OutPkt {
    pub flow: Flow,
    pub op: u16,
    pub flags: u32,
    pub data: Vec<u8>,
}

impl OutPkt {
    fn ctrl(flow: Flow, op: u16, flags: u32) -> Self {
        Self {
            flow,
            op,
            flags,
            data: Vec::new(),
        }
    }

    pub(crate) fn header(&self, guest_cid: u64, buf_alloc: u32, fwd_cnt: u32) -> [u8; HDR_LEN] {
        let mut raw = [0u8; HDR_LEN];
        raw[0..8].copy_from_slice(&HOST_CID.to_le_bytes());
        raw[8..16].copy_from_slice(&guest_cid.to_le_bytes());
        raw[16..20].copy_from_slice(&self.flow.host_port.to_le_bytes());
        raw[20..24].copy_from_slice(&self.flow.guest_port.to_le_bytes());
        raw[24..28].copy_from_slice(&(self.data.len() as u32).to_le_bytes());
        raw[28..30].copy_from_slice(&TYPE_STREAM.to_le_bytes());
        raw[30..32].copy_from_slice(&self.op.to_le_bytes());
        raw[32..36].copy_from_slice(&self.flags.to_le_bytes());
        raw[36..40].copy_from_slice(&buf_alloc.to_le_bytes());
        raw[40..44].copy_from_slice(&fwd_cnt.to_le_bytes());
        raw
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    WaitGuest,
    Established,
}

struct Conn {
    fd: std::fs::File,
    phase: Phase,
    connecting: bool,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    sent_to_guest: u32,
    received: u32,
    fwd_cnt: u32,
    last_fwd_sent: u32,
    buf_alloc: u32,
    to_host: Vec<u8>,
    ok_pending: Vec<u8>,
    prefetched: Vec<u8>,
    local_shut_send: bool,
    local_shut_recv: bool,
    peer_shut_send: bool,
    peer_shut_recv: bool,
    host_write_shut: bool,
    expiry: Option<std::time::Instant>,
}

impl Conn {
    fn new(fd: std::fs::File, phase: Phase) -> Self {
        Self {
            fd,
            phase,
            connecting: false,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            sent_to_guest: 0,
            received: 0,
            fwd_cnt: 0,
            last_fwd_sent: 0,
            buf_alloc: BUF_ALLOC,
            to_host: Vec::new(),
            ok_pending: Vec::new(),
            prefetched: Vec::new(),
            local_shut_send: false,
            local_shut_recv: false,
            peer_shut_send: false,
            peer_shut_recv: false,
            host_write_shut: false,
            expiry: None,
        }
    }

    fn peer_avail(&self) -> u32 {
        let inflight = self.sent_to_guest.wrapping_sub(self.peer_fwd_cnt);
        self.peer_buf_alloc.saturating_sub(inflight)
    }

    fn local_free(&self) -> u32 {
        let held = self.received.wrapping_sub(self.fwd_cnt);
        self.buf_alloc.saturating_sub(held)
    }
}

struct Incoming {
    fd: std::fs::File,
    buf: Vec<u8>,
}

enum ConnectResult {
    Done,
    Pending,
    Fail,
}

pub(crate) struct Mux {
    guest_cid: u64,
    path: std::path::PathBuf,
    listener: std::fs::File,
    incoming: Vec<Incoming>,
    conns: std::collections::HashMap<Flow, Conn>,
    next_host_port: u32,
    out_ctrl: std::collections::VecDeque<OutPkt>,
    out_data: std::collections::VecDeque<OutPkt>,
}

impl Mux {
    pub(crate) fn bind(guest_cid: u64, path: &std::path::Path) -> crate::error::Result<Self> {
        validate_guest_cid(guest_cid)?;
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_UDS_PATH {
            return Err(crate::error::Error::Vsock(
                "vsock socket path is empty or too long".into(),
            ));
        }
        if bytes.contains(&0) {
            return Err(crate::error::Error::Vsock(
                "vsock socket path contains an interior NUL".into(),
            ));
        }
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_socket() {
                std::fs::remove_file(path).map_err(|e| {
                    crate::error::Error::Vsock(format!("remove {}: {e}", path.display()))
                })?;
            } else {
                return Err(crate::error::Error::Vsock(format!(
                    "{} exists and is not a socket",
                    path.display()
                )));
            }
        }

        let listener =
            unix_stream().map_err(|e| crate::error::Error::Vsock(format!("vsock socket: {e}")))?;
        bind_unix(listener.as_raw_fd(), path)
            .map_err(|e| crate::error::Error::Vsock(format!("bind {}: {e}", path.display())))?;
        // SAFETY: listener is a socket we just created.
        let rc = unsafe { libc::listen(listener.as_raw_fd(), 128) };
        if rc < 0 {
            return Err(crate::error::Error::Vsock(format!(
                "listen {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }

        Ok(Self {
            guest_cid,
            path: path.to_path_buf(),
            listener,
            incoming: Vec::new(),
            conns: std::collections::HashMap::new(),
            next_host_port: 1024,
            out_ctrl: std::collections::VecDeque::new(),
            out_data: std::collections::VecDeque::new(),
        })
    }

    pub(crate) fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    pub(crate) fn has_out(&self) -> bool {
        !self.out_ctrl.is_empty() || !self.out_data.is_empty()
    }

    pub(crate) fn pop_out(&mut self) -> Option<OutPkt> {
        self.out_ctrl
            .pop_front()
            .or_else(|| self.out_data.pop_front())
    }

    pub(crate) fn push_front_out(&mut self, pkt: OutPkt) {
        if pkt.op == OP_RW {
            self.out_data.push_front(pkt);
        } else {
            self.out_ctrl.push_front(pkt);
        }
    }

    pub(crate) fn credit_of(&self, flow: Flow) -> (u32, u32) {
        self.conns
            .get(&flow)
            .map(|conn| (conn.buf_alloc, conn.fwd_cnt))
            .unwrap_or((0, 0))
    }

    /// `(fd, poll events)` for the listener and every host stream.
    pub(crate) fn interests(&self) -> Vec<(i32, i16)> {
        let mut out = Vec::with_capacity(1 + self.incoming.len() + self.conns.len());
        out.push((self.listener.as_raw_fd(), libc::POLLIN));
        for inc in &self.incoming {
            out.push((inc.fd.as_raw_fd(), libc::POLLIN));
        }
        for conn in self.conns.values() {
            let mut events = 0i16;
            if conn.connecting || !conn.ok_pending.is_empty() || !conn.to_host.is_empty() {
                events |= libc::POLLOUT;
            }
            let want_read = !conn.connecting
                && conn.ok_pending.is_empty()
                && !conn.local_shut_send
                && conn.prefetched.len() < MAX_PREFETCH;
            if want_read {
                events |= libc::POLLIN;
            }
            if events != 0 {
                out.push((conn.fd.as_raw_fd(), events));
            }
        }
        out
    }

    pub(crate) fn on_fd(&mut self, fd: i32, revents: i16) {
        if fd == self.listener.as_raw_fd() {
            self.accept_loop();
            return;
        }
        if let Some(idx) = self
            .incoming
            .iter()
            .position(|inc| inc.fd.as_raw_fd() == fd)
        {
            self.read_incoming(idx);
            return;
        }
        let Some(flow) = self
            .conns
            .iter()
            .find(|(_, conn)| conn.fd.as_raw_fd() == fd)
            .map(|(flow, _)| *flow)
        else {
            return;
        };
        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            self.read_conn(flow);
        }
        if self.conns.contains_key(&flow)
            && revents & (libc::POLLOUT | libc::POLLERR | libc::POLLNVAL) != 0
        {
            if self.conns.get(&flow).is_some_and(|conn| conn.connecting) {
                self.finish_connect(flow);
            } else {
                self.flush_conn(flow);
            }
        }
    }

    pub(crate) fn check_timeouts(&mut self, now: std::time::Instant) {
        let expired: Vec<Flow> = self
            .conns
            .iter()
            .filter(|(_, conn)| conn.expiry.is_some_and(|deadline| deadline <= now))
            .map(|(flow, _)| *flow)
            .collect();
        for flow in expired {
            self.drop_conn(flow, true);
        }
    }

    pub(crate) fn kill_connections(&mut self) {
        self.incoming.clear();
        self.conns.clear();
        self.out_ctrl.clear();
        self.out_data.clear();
    }

    pub(crate) fn reset_flow(&mut self, guest_port: u32, host_port: u32) {
        self.drop_conn(
            Flow {
                guest_port,
                host_port,
            },
            true,
        );
    }

    pub(crate) fn handle_guest_pkt(&mut self, hdr: &Header, data: &[u8]) {
        if hdr.src_cid != self.guest_cid || hdr.dst_cid != HOST_CID {
            return;
        }
        let flow = Flow {
            guest_port: hdr.src_port,
            host_port: hdr.dst_port,
        };
        if hdr.type_ != TYPE_STREAM {
            if hdr.op != OP_RST {
                self.enqueue_rst(flow);
            }
            return;
        }
        if hdr.op == OP_REQUEST {
            self.on_guest_request(flow, hdr);
            return;
        }
        if !self.conns.contains_key(&flow) {
            if hdr.op != OP_RST {
                self.enqueue_rst(flow);
            }
            return;
        }
        if let Some(conn) = self.conns.get_mut(&flow) {
            conn.peer_buf_alloc = hdr.buf_alloc;
            conn.peer_fwd_cnt = hdr.fwd_cnt;
        }
        match hdr.op {
            OP_RESPONSE => self.on_response(flow),
            OP_RST => self.drop_conn(flow, false),
            OP_SHUTDOWN => self.on_shutdown(flow, hdr.flags),
            OP_RW => {
                if data.len() > MAX_PAYLOAD {
                    self.drop_conn(flow, true);
                } else {
                    self.on_rw(flow, data);
                }
            }
            OP_CREDIT_REQUEST => self.enqueue_credit(flow),
            OP_CREDIT_UPDATE => {}
            _ => {}
        }
    }

    fn on_guest_request(&mut self, flow: Flow, hdr: &Header) {
        if self.conns.contains_key(&flow) {
            self.drop_conn(flow, true);
            return;
        }
        if hdr.len != 0 {
            self.enqueue_rst(flow);
            return;
        }
        let Some(path) = self.peer_socket_path(flow.host_port) else {
            self.enqueue_rst(flow);
            return;
        };
        let Ok(fd) = unix_stream() else {
            self.enqueue_rst(flow);
            return;
        };
        match connect_unix(fd.as_raw_fd(), &path) {
            ConnectResult::Done => {
                let mut conn = Conn::new(fd, Phase::Established);
                conn.peer_buf_alloc = hdr.buf_alloc;
                conn.peer_fwd_cnt = hdr.fwd_cnt;
                self.conns.insert(flow, conn);
                self.enqueue(OutPkt::ctrl(flow, OP_RESPONSE, 0));
            }
            ConnectResult::Pending => {
                let mut conn = Conn::new(fd, Phase::Established);
                conn.connecting = true;
                conn.peer_buf_alloc = hdr.buf_alloc;
                conn.peer_fwd_cnt = hdr.fwd_cnt;
                conn.expiry = Some(std::time::Instant::now() + REQUEST_TIMEOUT);
                self.conns.insert(flow, conn);
            }
            ConnectResult::Fail => self.enqueue_rst(flow),
        }
    }

    fn on_response(&mut self, flow: Flow) {
        let Some(conn) = self.conns.get_mut(&flow) else {
            return;
        };
        if conn.phase != Phase::WaitGuest {
            return;
        }
        conn.phase = Phase::Established;
        conn.expiry = None;
        conn.ok_pending = format!("OK {}\n", flow.host_port).into_bytes();
        self.flush_conn(flow);
    }

    fn on_shutdown(&mut self, flow: Flow, flags: u32) {
        let peer_recv = flags & SHUTDOWN_RCV != 0;
        let peer_send = flags & SHUTDOWN_SEND != 0;
        if let Some(conn) = self.conns.get_mut(&flow) {
            if peer_recv {
                conn.peer_shut_recv = true;
                conn.local_shut_send = true;
                shutdown_fd(conn.fd.as_raw_fd(), libc::SHUT_RD);
            }
            if peer_send {
                conn.peer_shut_send = true;
            }
        }
        self.flush_conn(flow);
    }

    fn on_rw(&mut self, flow: Flow, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let accept = self.conns.get(&flow).is_some_and(|conn| {
            conn.phase == Phase::Established && !conn.local_shut_recv && !conn.peer_shut_send
        });
        if !accept {
            return;
        }
        let free = self.conns.get(&flow).map(Conn::local_free).unwrap_or(0);
        if data.len() as u32 > free {
            self.drop_conn(flow, true);
            return;
        }
        if let Some(conn) = self.conns.get_mut(&flow) {
            conn.received = conn.received.wrapping_add(data.len() as u32);
            conn.to_host.extend_from_slice(data);
        }
        self.flush_conn(flow);
    }

    fn accept_loop(&mut self) {
        loop {
            // SAFETY: listener is a blocking-mode-cleared listening socket.
            let fd = unsafe {
                libc::accept4(
                    self.listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if fd < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if self.incoming.len() + self.conns.len() >= MAX_CONNS {
                // SAFETY: fd was just returned by accept4 and is not stored elsewhere.
                unsafe { libc::close(fd) };
                continue;
            }
            self.incoming.push(Incoming {
                // SAFETY: fd is an owned connected socket.
                fd: unsafe { std::fs::File::from_raw_fd(fd) },
                buf: Vec::new(),
            });
        }
    }

    fn read_incoming(&mut self, idx: usize) {
        let fd = self.incoming[idx].fd.as_raw_fd();
        let mut buf = [0u8; 64];
        match read_fd(fd, &mut buf) {
            Ok(0) => {
                self.incoming.remove(idx);
            }
            Ok(n) => {
                self.incoming[idx].buf.extend_from_slice(&buf[..n]);
                let ready = self.incoming[idx].buf.contains(&b'\n');
                let too_long = self.incoming[idx].buf.len() > 64 && !ready;
                if too_long {
                    self.incoming.remove(idx);
                } else if ready {
                    self.finish_incoming(idx);
                }
            }
            Err(err) if transient(&err) => {}
            Err(_) => {
                self.incoming.remove(idx);
            }
        }
    }

    fn finish_incoming(&mut self, idx: usize) {
        let inc = self.incoming.remove(idx);
        let Some(nl) = inc.buf.iter().position(|byte| *byte == b'\n') else {
            return;
        };
        let mut line = &inc.buf[..nl];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let rest = inc.buf[nl + 1..].to_vec();
        let Some(guest_port) = parse_connect(line) else {
            return;
        };
        let Some(host_port) = self.alloc_host_port() else {
            return;
        };
        let flow = Flow {
            guest_port,
            host_port,
        };
        if self.conns.contains_key(&flow) {
            self.enqueue_rst(flow);
            return;
        }
        let mut conn = Conn::new(inc.fd, Phase::WaitGuest);
        conn.prefetched = rest;
        conn.expiry = Some(std::time::Instant::now() + REQUEST_TIMEOUT);
        self.conns.insert(flow, conn);
        self.enqueue(OutPkt::ctrl(flow, OP_REQUEST, 0));
    }

    fn read_conn(&mut self, flow: Flow) {
        let Some(fd) = self.conns.get(&flow).map(|conn| conn.fd.as_raw_fd()) else {
            return;
        };
        let mut buf = [0u8; MAX_CHUNK];
        match read_fd(fd, &mut buf) {
            Ok(0) => self.on_host_eof(flow),
            Ok(n) => self.ingest(flow, &buf[..n]),
            Err(err) if transient(&err) => {}
            Err(_) => self.mark_local_shut(flow, true, true),
        }
    }

    fn on_host_eof(&mut self, flow: Flow) {
        let waiting = self
            .conns
            .get(&flow)
            .is_some_and(|conn| conn.phase == Phase::WaitGuest);
        if waiting {
            self.drop_conn(flow, true);
        } else {
            self.mark_local_shut(flow, false, true);
        }
    }

    fn ingest(&mut self, flow: Flow, data: &[u8]) {
        let mut rest = data;
        while !rest.is_empty() {
            let Some(conn) = self.conns.get(&flow) else {
                return;
            };
            if conn.phase != Phase::Established || conn.local_shut_send || conn.peer_shut_recv {
                if conn.prefetched.len() < MAX_PREFETCH {
                    let room = MAX_PREFETCH - conn.prefetched.len();
                    let n = rest.len().min(room);
                    if let Some(conn) = self.conns.get_mut(&flow) {
                        conn.prefetched.extend_from_slice(&rest[..n]);
                    }
                }
                return;
            }
            let credit = conn.peer_avail() as usize;
            if credit == 0 {
                self.stash_prefetch(flow, rest);
                self.request_credit(flow);
                return;
            }
            let n = rest.len().min(credit).min(MAX_CHUNK);
            let chunk = rest[..n].to_vec();
            rest = &rest[n..];
            if let Some(conn) = self.conns.get_mut(&flow) {
                conn.sent_to_guest = conn.sent_to_guest.wrapping_add(n as u32);
            }
            self.out_data.push_back(OutPkt {
                flow,
                op: OP_RW,
                flags: 0,
                data: chunk,
            });
        }
    }

    fn stash_prefetch(&mut self, flow: Flow, data: &[u8]) {
        let Some(conn) = self.conns.get_mut(&flow) else {
            return;
        };
        let room = MAX_PREFETCH.saturating_sub(conn.prefetched.len());
        let n = data.len().min(room);
        conn.prefetched.extend_from_slice(&data[..n]);
    }

    fn request_credit(&mut self, flow: Flow) {
        let already = self
            .out_ctrl
            .iter()
            .any(|pkt| pkt.flow == flow && pkt.op == OP_CREDIT_REQUEST);
        if already {
            return;
        }
        self.enqueue(OutPkt::ctrl(flow, OP_CREDIT_REQUEST, 0));
    }

    fn finish_connect(&mut self, flow: Flow) {
        let Some(fd) = self.conns.get(&flow).map(|conn| conn.fd.as_raw_fd()) else {
            return;
        };
        match socket_error(fd) {
            Ok(0) => {
                if let Some(conn) = self.conns.get_mut(&flow) {
                    conn.connecting = false;
                    conn.expiry = None;
                }
                self.enqueue(OutPkt::ctrl(flow, OP_RESPONSE, 0));
            }
            _ => self.drop_conn(flow, true),
        }
    }

    fn flush_conn(&mut self, flow: Flow) {
        let flushed = {
            let Some(conn) = self.conns.get_mut(&flow) else {
                return;
            };
            flush_ok(conn).and_then(|()| flush_to_host(conn))
        };
        match flushed {
            Ok(()) => {
                let shut_write = self.conns.get(&flow).is_some_and(|conn| {
                    conn.peer_shut_send && conn.to_host.is_empty() && !conn.host_write_shut
                });
                if shut_write && let Some(conn) = self.conns.get_mut(&flow) {
                    shutdown_fd(conn.fd.as_raw_fd(), libc::SHUT_WR);
                    conn.host_write_shut = true;
                }
                let prefetched = {
                    let Some(conn) = self.conns.get_mut(&flow) else {
                        return;
                    };
                    if !conn.ok_pending.is_empty() {
                        return;
                    }
                    std::mem::take(&mut conn.prefetched)
                };
                if !prefetched.is_empty() {
                    self.ingest(flow, &prefetched);
                }
                self.maybe_credit(flow);
            }
            Err(_) => self.mark_local_shut(flow, true, true),
        }
    }

    fn maybe_credit(&mut self, flow: Flow) {
        let Some(conn) = self.conns.get(&flow) else {
            return;
        };
        let delta = conn.fwd_cnt.wrapping_sub(conn.last_fwd_sent);
        let thresh = (conn.buf_alloc / 4).max(4096);
        if delta < thresh {
            return;
        }
        if self
            .out_ctrl
            .iter()
            .any(|pkt| pkt.flow == flow && pkt.op == OP_CREDIT_UPDATE)
        {
            return;
        }
        if let Some(conn) = self.conns.get_mut(&flow) {
            conn.last_fwd_sent = conn.fwd_cnt;
        }
        self.enqueue(OutPkt::ctrl(flow, OP_CREDIT_UPDATE, 0));
    }

    fn enqueue_credit(&mut self, flow: Flow) {
        if self
            .out_ctrl
            .iter()
            .any(|pkt| pkt.flow == flow && pkt.op == OP_CREDIT_UPDATE)
        {
            return;
        }
        if let Some(conn) = self.conns.get_mut(&flow) {
            conn.last_fwd_sent = conn.fwd_cnt;
        }
        self.enqueue(OutPkt::ctrl(flow, OP_CREDIT_UPDATE, 0));
    }

    fn mark_local_shut(&mut self, flow: Flow, recv: bool, send: bool) {
        let waiting = self
            .conns
            .get(&flow)
            .is_some_and(|conn| conn.phase == Phase::WaitGuest);
        if waiting {
            self.drop_conn(flow, true);
            return;
        }
        let mut flags = 0u32;
        if let Some(conn) = self.conns.get_mut(&flow) {
            if send && !conn.local_shut_send {
                conn.local_shut_send = true;
                flags |= SHUTDOWN_SEND;
            }
            if recv && !conn.local_shut_recv {
                conn.local_shut_recv = true;
                flags |= SHUTDOWN_RCV;
            }
            if flags != 0 {
                conn.expiry = Some(std::time::Instant::now() + SHUTDOWN_TIMEOUT);
            }
        }
        if flags != 0 {
            self.enqueue(OutPkt::ctrl(flow, OP_SHUTDOWN, flags));
        }
    }

    fn drop_conn(&mut self, flow: Flow, rst: bool) {
        self.conns.remove(&flow);
        self.out_ctrl.retain(|pkt| pkt.flow != flow);
        self.out_data.retain(|pkt| pkt.flow != flow);
        if rst {
            self.enqueue_rst(flow);
        }
    }

    fn enqueue_rst(&mut self, flow: Flow) {
        self.out_ctrl
            .retain(|pkt| !(pkt.flow == flow && pkt.op == OP_RST));
        self.enqueue(OutPkt::ctrl(flow, OP_RST, 0));
    }

    fn enqueue(&mut self, pkt: OutPkt) {
        if pkt.op == OP_RW {
            self.out_data.push_back(pkt);
        } else {
            self.out_ctrl.push_back(pkt);
        }
    }

    fn alloc_host_port(&mut self) -> Option<u32> {
        for _ in 0..60_000 {
            let port = self.next_host_port;
            self.next_host_port = if port == u32::MAX { 1024 } else { port + 1 };
            if port < 1024 {
                continue;
            }
            if self.conns.keys().any(|flow| flow.host_port == port) {
                continue;
            }
            return Some(port);
        }
        None
    }

    fn peer_socket_path(&self, port: u32) -> Option<std::path::PathBuf> {
        let mut name = self.path.clone().into_os_string();
        name.push(format!("_{port}"));
        if name.as_bytes().len() > 107 {
            None
        } else {
            Some(std::path::PathBuf::from(name))
        }
    }
}

impl Drop for Mux {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn parse_connect(line: &[u8]) -> Option<u32> {
    let rest = line.strip_prefix(b"CONNECT ")?;
    if rest.is_empty() || !rest.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let port: u32 = std::str::from_utf8(rest).ok()?.parse().ok()?;
    if port == 0 { None } else { Some(port) }
}

fn transient(err: &std::io::Error) -> bool {
    err.kind() == ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EINTR)
}

fn flush_ok(conn: &mut Conn) -> std::io::Result<()> {
    while !conn.ok_pending.is_empty() {
        match send_nosig(conn.fd.as_raw_fd(), &conn.ok_pending)? {
            0 => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "vsock handshake write returned 0",
                ));
            }
            n => conn.ok_pending.drain(..n),
        };
    }
    Ok(())
}

fn flush_to_host(conn: &mut Conn) -> std::io::Result<()> {
    while !conn.to_host.is_empty() {
        match send_nosig(conn.fd.as_raw_fd(), &conn.to_host)? {
            0 => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "vsock write returned 0",
                ));
            }
            n => {
                conn.to_host.drain(..n);
                conn.fwd_cnt = conn.fwd_cnt.wrapping_add(n as u32);
            }
        }
    }
    Ok(())
}

fn send_nosig(fd: i32, buf: &[u8]) -> std::io::Result<usize> {
    loop {
        // SAFETY: buf is a valid slice and fd is a socket owned by the caller.
        let n = unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err);
    }
}

fn read_fd(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        // SAFETY: buf is a valid writable slice and fd is owned by the caller.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err);
    }
}

fn shutdown_fd(fd: i32, how: libc::c_int) {
    // SAFETY: fd is a socket owned by the caller.
    unsafe {
        libc::shutdown(fd, how);
    }
}

fn socket_error(fd: i32) -> std::io::Result<i32> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: err and len are valid outputs for SO_ERROR.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut err as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(err)
    }
}

fn unix_stream() -> std::io::Result<std::fs::File> {
    // SAFETY: socket returns a new owned fd, or -1.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: fd is a newly created socket not shared with another owner.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

fn bind_unix(fd: i32, path: &std::path::Path) -> std::io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (idx, byte) in bytes.iter().enumerate() {
        addr.sun_path[idx] = *byte as libc::c_char;
    }
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: addr points at a sockaddr_un initialized above; fd is our socket.
    let rc = unsafe { libc::bind(fd, (&addr as *const libc::sockaddr_un).cast(), len) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn connect_unix(fd: i32, path: &std::path::Path) -> ConnectResult {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > 107 {
        return ConnectResult::Fail;
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (idx, byte) in bytes.iter().enumerate() {
        addr.sun_path[idx] = *byte as libc::c_char;
    }
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: addr is a sockaddr_un for path; fd is a nonblocking socket.
    let rc = unsafe { libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len) };
    if rc == 0 {
        ConnectResult::Done
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINPROGRESS) {
            ConnectResult::Pending
        } else {
            ConnectResult::Fail
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::io::Write as _;

    #[test]
    fn parse_connect_accepts_a_decimal_port() {
        assert_eq!(super::parse_connect(b"CONNECT 52"), Some(52));
        assert_eq!(super::parse_connect(b"CONNECT 0"), None);
        assert_eq!(super::parse_connect(b"CONNECT"), None);
        assert_eq!(super::parse_connect(b"CONNECT 12x"), None);
    }

    #[test]
    fn guest_cid_rejects_reserved_values() {
        assert!(super::validate_guest_cid(2).is_err());
        assert!(super::validate_guest_cid(0xffff_ffff).is_err());
        assert!(super::validate_guest_cid(3).is_ok());
    }

    #[test]
    fn header_roundtrip_keeps_ports_and_length() {
        let pkt = super::OutPkt {
            flow: super::Flow {
                guest_port: 52,
                host_port: 1024,
            },
            op: super::OP_RW,
            flags: 0,
            data: b"hi".to_vec(),
        };
        let raw = pkt.header(3, 4096, 7);
        let hdr = super::Header::parse(&raw);
        assert_eq!(hdr.src_cid, super::HOST_CID);
        assert_eq!(hdr.dst_cid, 3);
        assert_eq!(hdr.src_port, 1024);
        assert_eq!(hdr.dst_port, 52);
        assert_eq!(hdr.len, 2);
        assert_eq!(hdr.op, super::OP_RW);
        assert_eq!(hdr.buf_alloc, 4096);
        assert_eq!(hdr.fwd_cnt, 7);
    }

    fn temp_sock(name: &str) -> (std::path::PathBuf, super::Mux) {
        let dir = std::env::temp_dir().join(format!(
            "kitsune-vsock-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v.sock");
        let mux = super::Mux::bind(3, &path).unwrap();
        (path, mux)
    }

    fn pump(mux: &mut super::Mux) {
        // Newly accepted sockets are not in the poll set that accepted them.
        for _ in 0..8 {
            let interests = mux.interests();
            if interests.is_empty() {
                return;
            }
            let mut fds: Vec<libc::pollfd> = interests
                .into_iter()
                .map(|(fd, events)| libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                })
                .collect();
            // SAFETY: fds are sockets owned by the mux for this call.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
            if rc <= 0 {
                return;
            }
            for pfd in fds {
                if pfd.revents != 0 {
                    mux.on_fd(pfd.fd, pfd.revents);
                }
            }
        }
    }

    #[test]
    fn host_connect_waits_for_guest_response() {
        let (path, mut mux) = temp_sock("host");
        let mut host = std::os::unix::net::UnixStream::connect(&path).unwrap();
        host.write_all(b"CONNECT 52\n").unwrap();
        pump(&mut mux);
        let pkt = mux.pop_out().expect("request");
        assert_eq!(pkt.op, super::OP_REQUEST);
        assert_eq!(pkt.flow.guest_port, 52);
        let host_port = pkt.flow.host_port;

        let response = super::Header {
            src_cid: 3,
            dst_cid: super::HOST_CID,
            src_port: 52,
            dst_port: host_port,
            len: 0,
            type_: super::TYPE_STREAM,
            op: super::OP_RESPONSE,
            flags: 0,
            buf_alloc: 4,
            fwd_cnt: 0,
        };
        mux.handle_guest_pkt(&response, &[]);
        pump(&mut mux);
        host.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut ok = [0u8; 32];
        let n = host.read(&mut ok).unwrap();
        let text = std::str::from_utf8(&ok[..n]).unwrap();
        assert_eq!(text, format!("OK {host_port}\n"));

        host.write_all(b"abcdef").unwrap();
        pump(&mut mux);
        let data = std::iter::from_fn(|| mux.pop_out()).find(|pkt| pkt.op == super::OP_RW);
        let data = data.expect("rw");
        assert_eq!(data.data, b"abcd");
    }

    #[test]
    fn bad_connect_line_closes_the_host_socket() {
        let (path, mut mux) = temp_sock("bad");
        let mut host = std::os::unix::net::UnixStream::connect(&path).unwrap();
        host.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        host.write_all(b"HELLO\n").unwrap();
        pump(&mut mux);
        let mut buf = [0u8; 8];
        assert_eq!(host.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn guest_request_reaches_a_listening_unix_socket() {
        let (path, mut mux) = temp_sock("guest");
        let listen_at = {
            let mut name = path.clone().into_os_string();
            name.push("_52");
            std::path::PathBuf::from(name)
        };
        let listener = std::os::unix::net::UnixListener::bind(&listen_at).unwrap();
        let request = super::Header {
            src_cid: 3,
            dst_cid: super::HOST_CID,
            src_port: 9,
            dst_port: 52,
            len: 0,
            type_: super::TYPE_STREAM,
            op: super::OP_REQUEST,
            flags: 0,
            buf_alloc: 4096,
            fwd_cnt: 0,
        };
        mux.handle_guest_pkt(&request, &[]);
        pump(&mut mux);
        let pkt = mux.pop_out().expect("response");
        assert_eq!(pkt.op, super::OP_RESPONSE);
        assert_eq!(pkt.flow.guest_port, 9);
        assert_eq!(pkt.flow.host_port, 52);

        let rw = super::Header {
            op: super::OP_RW,
            len: 2,
            ..request
        };
        mux.handle_guest_pkt(&rw, b"hi");
        pump(&mut mux);
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8];
        sock.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let n = sock.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hi");
    }
}
