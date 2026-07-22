//! Legacy I/O used for guest reboot.

/// Guest request that should end the VMM run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    None,
    /// Guest requested a platform reset (treat as clean VMM exit).
    Reset,
}

/// i8042 command/status port (keyboard controller).
pub const I8042_DATA_PORT: u16 = 0x60;
pub const I8042_CMD_PORT: u16 = 0x64;

/// Pulse reset line (Linux `native_machine_emergency_restart` BOOT_KBD path).
const I8042_CMD_RESET_CPU: u8 = 0xfe;
/// Status bit: keyboard interface enabled (POST-ready default).
const I8042_STATUS_KBD_ENABLED: u8 = 0x10;

/// Intel ICH reset control register.
pub const CF9_PORT: u16 = 0xcf9;
/// RST_CPU: when set together with a full reset code, the platform resets.
const CF9_RST_CPU: u8 = 0x04;

/// Shared legacy I/O state (i8042 status defaults + last CF9 value).
#[derive(Debug)]
pub struct LegacyIo {
    i8042_status: u8,
    cf9: u8,
}

impl LegacyIo {
    pub fn new() -> Self {
        Self {
            // Input buffer empty (bit 1 clear) so `kb_wait` returns immediately.
            i8042_status: I8042_STATUS_KBD_ENABLED,
            cf9: 0,
        }
    }

    pub fn handles_port(port: u16) -> bool {
        matches!(port, I8042_DATA_PORT | I8042_CMD_PORT | CF9_PORT)
    }

    pub fn bus_read(&self, port: u16, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let value = match port {
            I8042_CMD_PORT => self.i8042_status,
            I8042_DATA_PORT => 0,
            CF9_PORT => self.cf9,
            _ => 0xff,
        };
        data[0] = value;
        data[1..].fill(0);
    }

    pub fn bus_write(&mut self, port: u16, data: &[u8]) -> PowerAction {
        let Some(&value) = data.first() else {
            return PowerAction::None;
        };
        match port {
            I8042_CMD_PORT if value == I8042_CMD_RESET_CPU => PowerAction::Reset,
            I8042_CMD_PORT | I8042_DATA_PORT => PowerAction::None,
            CF9_PORT => {
                self.cf9 = value;
                if value & CF9_RST_CPU != 0 {
                    PowerAction::Reset
                } else {
                    PowerAction::None
                }
            }
            _ => PowerAction::None,
        }
    }
}

impl Default for LegacyIo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn i8042_status_allows_kb_wait() {
        let io = super::LegacyIo::new();
        let mut data = [0xffu8];
        io.bus_read(super::I8042_CMD_PORT, &mut data);
        assert_eq!(data[0] & 0x02, 0, "IBF must be clear");
    }

    #[test]
    fn i8042_reset_command() {
        let mut io = super::LegacyIo::new();
        assert_eq!(
            io.bus_write(super::I8042_CMD_PORT, &[0xfe]),
            super::PowerAction::Reset
        );
        assert_eq!(
            io.bus_write(super::I8042_CMD_PORT, &[0xaa]),
            super::PowerAction::None
        );
        assert_eq!(
            io.bus_write(super::I8042_DATA_PORT, &[0xfe]),
            super::PowerAction::None
        );
    }

    #[test]
    fn cf9_reset_on_rst_cpu_bit() {
        let mut io = super::LegacyIo::new();
        // First Linux write: request hard reset (bit 1 only).
        assert_eq!(
            io.bus_write(super::CF9_PORT, &[0x02]),
            super::PowerAction::None
        );
        let mut data = [0u8];
        io.bus_read(super::CF9_PORT, &mut data);
        assert_eq!(data[0], 0x02);
        // Full reset codes 0x06 (warm) / 0x0e (cold).
        assert_eq!(
            io.bus_write(super::CF9_PORT, &[0x06]),
            super::PowerAction::Reset
        );
        assert_eq!(
            io.bus_write(super::CF9_PORT, &[0x0e]),
            super::PowerAction::Reset
        );
    }

    #[test]
    fn handles_expected_ports() {
        assert!(super::LegacyIo::handles_port(0x60));
        assert!(super::LegacyIo::handles_port(0x64));
        assert!(super::LegacyIo::handles_port(0xcf9));
        assert!(!super::LegacyIo::handles_port(0x3f8));
    }
}
