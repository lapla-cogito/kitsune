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

/// 16550-compatible serial console writing guest output to the host stdout.
pub struct SerialConsole {
    inner: vm_superio::Serial<NoopTrigger, vm_superio::serial::NoEvents, std::io::Stdout>,
}

impl SerialConsole {
    pub fn new() -> Self {
        Self {
            inner: vm_superio::Serial::new(NoopTrigger, std::io::stdout()),
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

    pub fn handles_port(port: u16) -> bool {
        (SERIAL_PORT_BASE..SERIAL_PORT_BASE + SERIAL_PORT_SIZE).contains(&port)
    }
}

impl Default for SerialConsole {
    fn default() -> Self {
        Self::new()
    }
}
