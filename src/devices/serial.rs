/// COM1 base port.
pub const SERIAL_PORT_BASE: u16 = 0x3f8;
/// Number of I/O ports used by the 16550 UART.
pub const SERIAL_PORT_SIZE: u16 = 8;

/// Interrupt trigger that does nothing (used before irqchip wiring).
#[derive(Debug)]
struct NoopTrigger;

impl vm_superio::Trigger for NoopTrigger {
    type E = std::io::Error;

    fn trigger(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 16550-compatible serial console bridged to the host stdin/stdout.
pub struct SerialConsole {
    inner: vm_superio::Serial<NoopTrigger, vm_superio::serial::NoEvents, std::io::Stdout>,
    stdin_ready: bool,
    stdin_flags: Option<libc::c_int>,
}

impl SerialConsole {
    pub fn new() -> Self {
        let mut stdin_ready = false;
        let mut stdin_flags = None;

        // SAFETY: STDIN_FILENO is a valid process fd.
        let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        if flags >= 0 {
            // SAFETY: same fd; only add O_NONBLOCK.
            let rc =
                unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            if rc == 0 {
                stdin_ready = true;
                stdin_flags = Some(flags);
            }
        }

        Self {
            inner: vm_superio::Serial::new(NoopTrigger, std::io::stdout()),
            stdin_ready,
            stdin_flags,
        }
    }

    /// Handle a guest write to a port in the COM1 range.
    pub fn bus_write(&mut self, port: u16, data: &[u8]) -> crate::error::Result<()> {
        let offset = (port - SERIAL_PORT_BASE) as u8;
        for &byte in data {
            self.inner
                .write(offset, byte)
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

    /// Pull available bytes from host stdin into the UART RX FIFO.
    pub fn poll_stdin(&mut self) -> crate::error::Result<()> {
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
                self.inner
                    .enqueue_raw_bytes(&buf[..n])
                    .map_err(|e| crate::error::Error::Serial(e.to_string()))?;
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

impl Default for SerialConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SerialConsole {
    fn drop(&mut self) {
        if let Some(flags) = self.stdin_flags.take() {
            // SAFETY: restore flags previously read from the same fd.
            unsafe {
                libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags);
            }
        }
    }
}
