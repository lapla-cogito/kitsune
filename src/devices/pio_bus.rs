//! Port I/O dispatch for the serial console and legacy power-management registers.

#[derive(Clone)]
pub(crate) struct PioBus {
    serial: std::sync::Arc<std::sync::Mutex<super::serial::SerialConsole>>,
    legacy: std::sync::Arc<std::sync::Mutex<super::legacy_io::LegacyIo>>,
}

impl PioBus {
    pub(crate) fn new(vm: &kvm_ioctls::VmFd) -> crate::error::Result<Self> {
        Ok(Self {
            serial: std::sync::Arc::new(std::sync::Mutex::new(super::serial::SerialConsole::new(
                vm,
            )?)),
            legacy: std::sync::Arc::new(std::sync::Mutex::new(super::legacy_io::LegacyIo::new())),
        })
    }

    pub(crate) fn serial(&self) -> std::sync::Arc<std::sync::Mutex<super::serial::SerialConsole>> {
        std::sync::Arc::clone(&self.serial)
    }

    pub(crate) fn read(&self, port: u16, data: &mut [u8]) {
        if super::serial::SerialConsole::handles_port(port) {
            self.serial
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bus_read(port, data);
        } else if super::legacy_io::LegacyIo::handles_port(port) {
            self.legacy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bus_read(port, data);
        } else {
            data.fill(0xff);
        }
    }

    pub(crate) fn write(
        &self,
        port: u16,
        data: &[u8],
    ) -> crate::error::Result<super::legacy_io::PowerAction> {
        if super::serial::SerialConsole::handles_port(port) {
            self.serial
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bus_write(port, data)?;
            Ok(super::legacy_io::PowerAction::None)
        } else if super::legacy_io::LegacyIo::handles_port(port) {
            Ok(self
                .legacy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bus_write(port, data))
        } else {
            Ok(super::legacy_io::PowerAction::None)
        }
    }
}
