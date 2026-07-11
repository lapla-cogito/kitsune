//! kitsune: a KVM-based virtual machine monitor.

mod acpi;
mod boot;
mod config;
mod devices;
mod error;
mod gdt;
mod memory;
mod vcpu;
mod vmm;

pub use boot::KernelBootConfig;
pub use config::{DEFAULT_KERNEL_CMDLINE, INITRD_CMDLINE_EXTRA, MAX_VCPUS, VmmConfig};
pub use devices::VirtioBlock;
pub use error::{Error, Result};
pub use vmm::Vmm;
