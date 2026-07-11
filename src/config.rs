/// Maximum number of guest vCPUs kitsune will create.
pub const MAX_VCPUS: u8 = 32;

/// Configuration for a virtual machine.
#[derive(Debug, Clone)]
pub struct VmmConfig {
    /// Guest memory size in bytes. Must be a non-zero multiple of 4096.
    pub mem_size: usize,
    /// Number of virtual CPUs (1 ..= [`MAX_VCPUS`]).
    pub num_vcpus: u8,
}

impl Default for VmmConfig {
    fn default() -> Self {
        Self {
            mem_size: 256 * 1024 * 1024,
            num_vcpus: 1,
        }
    }
}

/// Default kernel command line for serial console boot without PCI.
pub const DEFAULT_KERNEL_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 pci=off nomodule";

/// Extra cmdline token used when an initrd is provided.
pub const INITRD_CMDLINE_EXTRA: &str = "rdinit=/init";
