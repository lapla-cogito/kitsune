/// Configuration for a virtual machine.
#[derive(Debug, Clone)]
pub struct VmmConfig {
    /// Guest memory size in bytes. Must be a non-zero multiple of 4096.
    pub mem_size: usize,
}

impl Default for VmmConfig {
    fn default() -> Self {
        Self {
            mem_size: 256 * 1024 * 1024,
        }
    }
}

/// Default kernel command line for serial console boot without PCI.
pub const DEFAULT_KERNEL_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 pci=off nomodule";
