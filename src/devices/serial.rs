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

/// 16550-compatible serial console bridged to the host stdin/stdout.
pub struct SerialConsole {
    inner: vm_superio::Serial<IrqfdTrigger, vm_superio::serial::NoEvents, std::io::Stdout>,
    irq_fd: std::sync::Arc<vmm_sys_util::eventfd::EventFd>,
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

        Ok(Self {
            inner: vm_superio::Serial::new(
                IrqfdTrigger {
                    fd: std::sync::Arc::clone(&irq_fd),
                },
                std::io::stdout(),
            ),
            irq_fd,
            stdin_ready,
            host,
            dsr_probe: DsrProbe::Idle,
            pending_rx: Vec::new(),
            dsr_auto_reply,
        })
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

    /// Handle a guest read from a port in the COM1 range.
    pub fn bus_read(&mut self, port: u16, data: &mut [u8]) {
        let offset = (port - SERIAL_PORT_BASE) as u8;
        for byte in data.iter_mut() {
            *byte = self.inner.read(offset);
        }
    }

    /// Pull host stdin into the UART RX FIFO; inject pending ANSI replies.
    pub fn poll_stdin(&mut self) -> crate::error::Result<()> {
        self.flush_pending_rx()?;
        self.reassert_rx_irq()?;

        if !self.stdin_ready {
            return Ok(());
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
        Ok(())
    }

    pub fn handles_port(port: u16) -> bool {
        (SERIAL_PORT_BASE..SERIAL_PORT_BASE + SERIAL_PORT_SIZE).contains(&port)
    }
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
