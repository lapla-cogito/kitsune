//! COM1 (16550) serial console bridged to host stdin/stdout.

/// COM1 base port.
pub const SERIAL_PORT_BASE: u16 = 0x3f8;
/// Number of I/O ports used by the 16550 UART.
pub const SERIAL_PORT_SIZE: u16 = 8;
/// ISA IRQ line for COM1 (identity-mapped GSI on KVM's in-kernel irqchip).
pub const SERIAL_IRQ: u32 = 4;

/// vm-superio RX FIFO depth (must match the crate's private FIFO_SIZE).
const UART_FIFO_SIZE: usize = 0x40;

/// Raises COM1's GSI through a KVM irqfd whenever the UART needs an interrupt.
#[derive(Debug)]
struct IrqfdTrigger {
    fd: std::sync::Arc<vmm_sys_util::eventfd::EventFd>,
}

impl vm_superio::Trigger for IrqfdTrigger {
    type E = std::io::Error;

    fn trigger(&self) -> std::io::Result<()> {
        self.fd.write(1).map(|_| ())
    }
}

/// Tracks guest TX for the ANSI Device Status Report query `ESC [ 6 n`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum DsrProbe {
    #[default]
    Idle,
    Esc,
    Bracket,
    Six,
}

/// Host stdin state restored on drop (and from signal handlers).
struct HostStdioState {
    fcntl_flags: Option<libc::c_int>,
    termios: Option<libc::termios>,
}

/// Result of one [`SerialConsole::drain_stdin`] pass for the stdin worker.
#[derive(Debug, Clone, Copy)]
pub struct DrainStatus {
    /// Host stdin is still open for reading.
    pub stdin_open: bool,
    /// UART RX FIFO has room for more host bytes.
    pub fifo_has_space: bool,
}

/// 16550-compatible serial console bridged to the host stdin/stdout.
pub struct SerialConsole {
    inner: vm_superio::Serial<IrqfdTrigger, vm_superio::serial::NoEvents, std::io::Stdout>,
    irq_fd: std::sync::Arc<vmm_sys_util::eventfd::EventFd>,
    /// Wakes the stdin worker when RX FIFO has space after being full.
    rx_space_fd: vmm_sys_util::eventfd::EventFd,
    stdin_ready: bool,
    host: HostStdioState,
    dsr_probe: DsrProbe,
    pending_rx: Vec<u8>,
    /// Auto-answer CSI 6n only when host stdout will not (not a TTY).
    dsr_auto_reply: bool,
}

impl SerialConsole {
    /// Create a serial console and register COM1 (`SERIAL_IRQ`) with KVM.
    pub fn new(vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        let host = prepare_host_stdio()?;
        let stdin_ready = host.fcntl_flags.is_some();
        // SAFETY: STDOUT_FILENO is a valid process fd.
        let dsr_auto_reply = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 0;

        let irq_fd = std::sync::Arc::new(
            vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
                .map_err(|e| crate::error::Error::Serial(format!("serial irq eventfd: {e}")))?,
        );
        vm.register_irqfd(irq_fd.as_ref(), SERIAL_IRQ)
            .map_err(crate::error::Error::KvmIoctl)?;

        let rx_space_fd = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| crate::error::Error::Serial(format!("serial rx space eventfd: {e}")))?;

        Ok(Self {
            inner: vm_superio::Serial::new(
                IrqfdTrigger {
                    fd: std::sync::Arc::clone(&irq_fd),
                },
                std::io::stdout(),
            ),
            irq_fd,
            rx_space_fd,
            stdin_ready,
            host,
            dsr_probe: DsrProbe::Idle,
            pending_rx: Vec::new(),
            dsr_auto_reply,
        })
    }

    /// Clone of the RX-space eventfd for the stdin worker.
    pub fn rx_space_fd(&self) -> crate::error::Result<vmm_sys_util::eventfd::EventFd> {
        self.rx_space_fd
            .try_clone()
            .map_err(|e| crate::error::Error::Serial(e.to_string()))
    }

    /// Handle a guest write to a port in the COM1 range.
    pub fn bus_write(&mut self, port: u16, data: &[u8]) -> crate::error::Result<()> {
        let offset = (port - SERIAL_PORT_BASE) as u8;
        let is_data = offset == 0;
        for &byte in data {
            self.write_uart(offset, byte)?;
            if is_data {
                self.observe_guest_tx(byte)?;
            }
        }

        if is_data && !data.is_empty() {
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
        }
        Ok(())
    }

    fn write_uart(&mut self, offset: u8, byte: u8) -> crate::error::Result<()> {
        loop {
            match self.inner.write(offset, byte) {
                Ok(()) => return Ok(()),
                Err(vm_superio::serial::Error::IOError(e))
                    if e.kind() == std::io::ErrorKind::Interrupted =>
                {
                    continue;
                }
                Err(e) => return Err(crate::error::Error::Serial(e.to_string())),
            }
        }
    }

    fn observe_guest_tx(&mut self, byte: u8) -> crate::error::Result<()> {
        self.dsr_probe = match (self.dsr_probe, byte) {
            (_, 0x1b) => DsrProbe::Esc,
            (DsrProbe::Esc, b'[') => DsrProbe::Bracket,
            (DsrProbe::Bracket, b'6') => DsrProbe::Six,
            (DsrProbe::Six, b'n') => {
                if self.dsr_auto_reply {
                    self.pending_rx.extend_from_slice(b"\x1b[1;80R");
                    self.flush_pending_rx()?;
                    self.reassert_rx_irq()?;
                    self.notify_rx_space();
                }
                DsrProbe::Idle
            }
            _ => DsrProbe::Idle,
        };
        Ok(())
    }

    fn enqueue_rx(&mut self, bytes: &[u8]) -> crate::error::Result<()> {
        self.inner
            .enqueue_raw_bytes(bytes)
            .map_err(|e| crate::error::Error::Serial(e.to_string()))?;
        Ok(())
    }

    fn flush_pending_rx(&mut self) -> crate::error::Result<()> {
        while !self.pending_rx.is_empty() {
            let capacity = self.inner.fifo_capacity();
            if capacity == 0 {
                break;
            }
            let n = capacity.min(self.pending_rx.len());
            match self.inner.enqueue_raw_bytes(&self.pending_rx[..n]) {
                Ok(written) => {
                    self.pending_rx.drain(..written);
                }
                Err(e) => return Err(crate::error::Error::Serial(e.to_string())),
            }
        }
        Ok(())
    }

    fn reassert_rx_irq(&self) -> crate::error::Result<()> {
        if self.inner.fifo_capacity() < UART_FIFO_SIZE {
            self.irq_fd
                .write(1)
                .map_err(|e| crate::error::Error::Serial(e.to_string()))?;
        }
        Ok(())
    }

    fn notify_rx_space(&self) {
        if self.inner.fifo_capacity() > 0 {
            let _ = self.rx_space_fd.write(1);
        }
    }

    /// Handle a guest read from a port in the COM1 range.
    pub fn bus_read(&mut self, port: u16, data: &mut [u8]) {
        let offset = (port - SERIAL_PORT_BASE) as u8;
        for byte in data.iter_mut() {
            *byte = self.inner.read(offset);
        }
        // Guest drain of RBR frees RX FIFO space for the host stdin worker.
        if offset == 0 {
            self.notify_rx_space();
        }
    }

    /// Pull available host stdin bytes into the UART RX FIFO.
    pub fn drain_stdin(&mut self) -> crate::error::Result<DrainStatus> {
        self.flush_pending_rx()?;
        self.reassert_rx_irq()?;

        if !self.stdin_ready {
            return Ok(DrainStatus {
                stdin_open: false,
                fifo_has_space: self.inner.fifo_capacity() > 0,
            });
        }

        let mut buf = [0u8; 64];
        loop {
            let capacity = self.inner.fifo_capacity();
            if capacity == 0 {
                break;
            }
            let want = capacity.min(buf.len());

            // SAFETY: reading into a valid stack buffer from STDIN_FILENO.
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    want,
                )
            };
            if n > 0 {
                let n = n as usize;
                self.enqueue_rx(&buf[..n])?;
            } else if n == 0 {
                self.stdin_ready = false;
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(crate::error::Error::Serial(err.to_string()));
            }
        }

        Ok(DrainStatus {
            stdin_open: self.stdin_ready,
            fifo_has_space: self.inner.fifo_capacity() > 0,
        })
    }

    pub fn stdin_ready(&self) -> bool {
        self.stdin_ready
    }

    pub fn handles_port(port: u16) -> bool {
        (SERIAL_PORT_BASE..SERIAL_PORT_BASE + SERIAL_PORT_SIZE).contains(&port)
    }
}

/// Background thread that blocks in `poll` on stdin and feeds [`SerialConsole`].
pub struct StdinWorker {
    stop_fd: vmm_sys_util::eventfd::EventFd,
    handle: Option<std::thread::JoinHandle<crate::error::Result<()>>>,
}

impl StdinWorker {
    /// Spawn the worker. `serial` is shared with the vCPU I/O path.
    pub fn start(
        serial: std::sync::Arc<std::sync::Mutex<SerialConsole>>,
    ) -> crate::error::Result<Self> {
        let stop_fd = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| crate::error::Error::Serial(e.to_string()))?;
        let stop_fd_worker = stop_fd
            .try_clone()
            .map_err(|e| crate::error::Error::Serial(e.to_string()))?;
        let space_fd = serial
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rx_space_fd()?;

        let handle = std::thread::Builder::new()
            .name("serial-stdin".into())
            .spawn(move || stdin_worker_loop(serial, stop_fd_worker, space_fd))
            .map_err(|e| crate::error::Error::Serial(e.to_string()))?;

        Ok(Self {
            stop_fd,
            handle: Some(handle),
        })
    }

    /// Wake the worker and wait for it to exit.
    pub fn stop(mut self) -> crate::error::Result<()> {
        let _ = self.stop_fd.write(1);
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        match handle.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(crate::error::Error::Serial(
                "serial stdin worker panicked".into(),
            )),
        }
    }
}

impl Drop for StdinWorker {
    fn drop(&mut self) {
        let _ = self.stop_fd.write(1);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn stdin_worker_loop(
    serial: std::sync::Arc<std::sync::Mutex<SerialConsole>>,
    stop_fd: vmm_sys_util::eventfd::EventFd,
    space_fd: vmm_sys_util::eventfd::EventFd,
) -> crate::error::Result<()> {
    use std::os::fd::AsRawFd as _;

    let stop_raw = stop_fd.as_raw_fd();
    let space_raw = space_fd.as_raw_fd();
    let mut watch_stdin = serial
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .stdin_ready();
    // When the UART RX FIFO is full, do not poll stdin (level-triggered would spin).
    let mut wait_for_space = false;

    loop {
        // fds[0]=stop, fds[1]=stdin or space, depending on wait_for_space.
        let mut fds = [
            libc::pollfd {
                fd: stop_raw,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let mut nfds = 1usize;
        if wait_for_space {
            fds[1].fd = space_raw;
            nfds = 2;
        } else if watch_stdin {
            fds[1].fd = libc::STDIN_FILENO;
            nfds = 2;
        }

        // SAFETY: poll on valid fds owned for the worker lifetime.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), nfds as libc::nfds_t, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(crate::error::Error::Serial(format!("stdin poll: {err}")));
        }

        if fds[0].revents != 0 {
            let _ = stop_fd.read();
            break;
        }

        if nfds < 2 || fds[1].revents == 0 {
            continue;
        }

        let rev = fds[1].revents;
        if wait_for_space {
            // RX FIFO may have space; clear and try draining stdin again.
            if rev & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(crate::error::Error::Serial(
                    "serial rx space eventfd error".into(),
                ));
            }
            let _ = space_fd.read();
            wait_for_space = false;
            let status = {
                let mut guard = serial.lock().unwrap_or_else(|e| e.into_inner());
                guard.drain_stdin()?
            };
            watch_stdin = status.stdin_open;
            if watch_stdin && !status.fifo_has_space {
                wait_for_space = true;
                let _ = space_fd.read(); // clear stale signals before waiting
            }
            continue;
        }

        // Watching stdin.
        if rev & (libc::POLLERR | libc::POLLNVAL) != 0 {
            watch_stdin = false;
            continue;
        }
        // POLLHUP without POLLIN: peer closed; treat as EOF.
        if rev & libc::POLLHUP != 0 && rev & libc::POLLIN == 0 {
            let mut guard = serial.lock().unwrap_or_else(|e| e.into_inner());
            let _ = guard.drain_stdin()?;
            watch_stdin = false;
            continue;
        }
        if rev & libc::POLLIN == 0 {
            continue;
        }

        let status = {
            let mut guard = serial.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain_stdin()?
        };
        watch_stdin = status.stdin_open;
        if watch_stdin && !status.fifo_has_space {
            wait_for_space = true;
            let _ = space_fd.read();
        }
    }
    Ok(())
}

fn prepare_host_stdio() -> crate::error::Result<HostStdioState> {
    let mut fcntl_flags = None;
    let mut termios_saved = None;

    // SAFETY: STDIN_FILENO is a valid process fd.
    let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
    if flags >= 0 {
        // SAFETY: only adds O_NONBLOCK.
        let rc =
            unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if rc == 0 {
            fcntl_flags = Some(flags);
        }
    }

    // Disable host echo so only the guest paints typed characters.
    // SAFETY: termios for the process controlling terminal when stdin is a TTY.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 0 {
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut tio) } == 0 {
            let original = tio;
            unsafe { libc::cfmakeraw(&mut tio) };
            // Keep ISIG so Ctrl-C still generates SIGINT on the host.
            tio.c_lflag |= libc::ISIG;
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &tio) } == 0 {
                termios_saved = Some(original);
                // SAFETY: single-threaded setup before the run loop; used by signals.
                unsafe {
                    STDIO_SAVED_FLAGS = flags;
                    STDIO_SAVED_TERMIOS = original;
                    STDIO_HAS_BACKUP = true;
                }
                install_stdio_signal_handlers();
            }
        }
    }

    Ok(HostStdioState {
        fcntl_flags,
        termios: termios_saved,
    })
}

// Written only during SerialConsole::new; read from signal handlers / Drop.
static mut STDIO_HAS_BACKUP: bool = false;
static mut STDIO_SAVED_FLAGS: libc::c_int = 0;
static mut STDIO_SAVED_TERMIOS: libc::termios = unsafe { std::mem::zeroed() };

fn install_stdio_signal_handlers() {
    // SAFETY: no-op-style restore handler without SA_RESTART.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = restore_stdio_then_reraise as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

fn restore_host_stdio(fcntl_flags: Option<libc::c_int>, termios: Option<&libc::termios>) {
    if let Some(flags) = fcntl_flags {
        // SAFETY: restore flags previously read from STDIN_FILENO.
        unsafe {
            libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags);
        }
    }
    if let Some(tio) = termios {
        // SAFETY: restore termios previously read from STDIN_FILENO.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, tio);
        }
    }
}

extern "C" fn restore_stdio_then_reraise(sig: libc::c_int) {
    // SAFETY: backup fields are set once before handlers are installed.
    unsafe {
        if STDIO_HAS_BACKUP {
            let flags = STDIO_SAVED_FLAGS;
            let tio = STDIO_SAVED_TERMIOS;
            restore_host_stdio(Some(flags), Some(&tio));
            STDIO_HAS_BACKUP = false;
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

impl Drop for SerialConsole {
    fn drop(&mut self) {
        // SAFETY: clear signal-handler backup for the normal exit path.
        unsafe {
            STDIO_HAS_BACKUP = false;
        }
        restore_host_stdio(self.host.fcntl_flags.take(), self.host.termios.as_ref());
    }
}
