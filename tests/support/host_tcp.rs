//! Host-side TCP responder for guest to host virtio-net e2e.

pub struct HostTcpService {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

pub const DEFAULT_ADDR: &str = "192.168.77.1:7777";
pub const REPLY: &[u8] = b"kitsune-host-tcp-ok\n";

impl HostTcpService {
    /// Start a background accept loop. Fails if bind fails (TAP IP not configured yet).
    pub fn start(addr: &str) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_t = std::sync::Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("e2e-host-tcp".into())
            .spawn(move || {
                while !stop_t.load(std::sync::atomic::Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = std::io::Write::write_all(&mut stream, REPLY);
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for HostTcpService {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
