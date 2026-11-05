//! Legacy I/O used for guest reboot and poweroff.

/// Guest request that should end the VMM run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    None,
    /// Guest requested a platform reset (treat as clean VMM exit).
    Reset,
    /// Guest requested soft-off (ACPI S5). Clean VMM exit.
    Shutdown,
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

/// HW-reduced ACPI Sleep Control register (FADT `SLEEP_CONTROL_REG`).
pub const SLEEP_CONTROL_PORT: u16 = 0x600;
/// HW-reduced ACPI Sleep Status register (FADT `SLEEP_STATUS_REG`).
pub const SLEEP_STATUS_PORT: u16 = 0x601;

/// `\_S5` SLP_TYPx value. The field is 3 bits wide.
pub const SLEEP_TYP_S5: u8 = 5;

const _: () = assert!(SLEEP_TYP_S5 <= 7);

/// Sleep Control bits [4:2], `ACPI_X_SLEEP_TYPE_MASK`.
const SLEEP_TYPE_MASK: u8 = 0x1c;
/// Sleep Control SLP_TYPx shift, `ACPI_X_SLEEP_TYPE_POSITION`.
const SLEEP_TYPE_SHIFT: u8 = 2;
/// Sleep Control bit 5, `ACPI_X_SLEEP_ENABLE`.
const SLEEP_ENABLE: u8 = 0x20;
/// Sleep Status bit 7, `ACPI_X_WAKE_STATUS` (write 1 to clear).
const SLEEP_WAKE_STATUS: u8 = 0x80;

/// Shared legacy I/O state (i8042, CF9, ACPI sleep registers).
#[derive(Debug)]
pub struct LegacyIo {
    i8042_status: u8,
    cf9: u8,
    sleep_control: u8,
    sleep_status: u8,
}

impl LegacyIo {
    pub fn new() -> Self {
        Self {
            // Input buffer empty (bit 1 clear) so `kb_wait` returns immediately.
            i8042_status: I8042_STATUS_KBD_ENABLED,
            cf9: 0,
            sleep_control: 0,
            sleep_status: 0,
        }
    }

    pub fn handles_port(port: u16) -> bool {
        matches!(
            port,
            I8042_DATA_PORT | I8042_CMD_PORT | CF9_PORT | SLEEP_CONTROL_PORT | SLEEP_STATUS_PORT
        )
    }

    pub fn bus_read(&self, port: u16, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let value = match port {
            I8042_CMD_PORT => self.i8042_status,
            I8042_DATA_PORT => 0,
            CF9_PORT => self.cf9,
            SLEEP_CONTROL_PORT => self.sleep_control,
            SLEEP_STATUS_PORT => self.sleep_status,
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
            SLEEP_CONTROL_PORT => {
                self.sleep_control = value;
                sleep_control_action(value)
            }
            SLEEP_STATUS_PORT => {
                // WAK_STS is write-1-to-clear. ACPICA clears it before SLP_EN.
                self.sleep_status &= !(value & SLEEP_WAKE_STATUS);
                PowerAction::None
            }
            _ => PowerAction::None,
        }
    }
}

/// `SLP_EN` plus the `\_S5` sleep type is soft-off.
///
/// Linux computes this as `(SLP_TYPa << 2) | ACPI_X_SLEEP_ENABLE`.
fn sleep_control_action(value: u8) -> PowerAction {
    if value & SLEEP_ENABLE == 0 {
        return PowerAction::None;
    }
    let typ = (value & SLEEP_TYPE_MASK) >> SLEEP_TYPE_SHIFT;
    if typ == SLEEP_TYP_S5 {
        PowerAction::Shutdown
    } else {
        PowerAction::None
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
        assert!(super::LegacyIo::handles_port(super::SLEEP_CONTROL_PORT));
        assert!(super::LegacyIo::handles_port(super::SLEEP_STATUS_PORT));
        assert!(!super::LegacyIo::handles_port(0x3f8));
    }

    #[test]
    fn sleep_control_s5_shuts_down() {
        let mut io = super::LegacyIo::new();
        // Same expression as ACPICA `acpi_hw_extended_sleep`.
        let value = ((super::SLEEP_TYP_S5 << super::SLEEP_TYPE_SHIFT) & super::SLEEP_TYPE_MASK)
            | super::SLEEP_ENABLE;
        assert_eq!(value, 0x34);
        assert_eq!(
            io.bus_write(super::SLEEP_CONTROL_PORT, &[value]),
            super::PowerAction::Shutdown
        );
        let mut data = [0u8; 2];
        io.bus_read(super::SLEEP_CONTROL_PORT, &mut data);
        assert_eq!(data, [value, 0]);
    }

    #[test]
    fn sleep_enable_without_s5_type_is_ignored() {
        let mut io = super::LegacyIo::new();
        // SLP_TYP 1 with SLP_EN. Not a state this VMM can enter.
        assert_eq!(
            io.bus_write(super::SLEEP_CONTROL_PORT, &[0x24]),
            super::PowerAction::None
        );
        // S5 type without SLP_EN is not a request to leave S0.
        assert_eq!(
            io.bus_write(super::SLEEP_CONTROL_PORT, &[0x14]),
            super::PowerAction::None
        );
    }

    #[test]
    fn sleep_status_wake_bit_is_write_1_to_clear() {
        let mut io = super::LegacyIo::new();
        assert_eq!(
            io.bus_write(super::SLEEP_STATUS_PORT, &[super::SLEEP_WAKE_STATUS]),
            super::PowerAction::None
        );
        let mut data = [0xffu8];
        io.bus_read(super::SLEEP_STATUS_PORT, &mut data);
        assert_eq!(data[0] & super::SLEEP_WAKE_STATUS, 0);
    }
}
