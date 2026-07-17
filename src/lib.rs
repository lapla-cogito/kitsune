//! kitsune: a KVM-based virtual machine monitor.

mod acpi;
mod boot;
mod cmdline;
mod config;
mod devices;
mod error;
mod gdt;
mod memory;
mod vcpu;
mod vmm;

pub use boot::KernelBootConfig;
pub use cmdline::KernelCmdlineOpts;
pub use cmdline::build_kernel_cmdline;
pub use config::DEFAULT_KERNEL_CMDLINE;
pub use config::INITRD_CMDLINE_EXTRA;
pub use config::MAX_VCPUS;
pub use config::VmmConfig;
pub use devices::VirtioBlock;
pub use devices::VirtioNet;
pub use error::Error;
pub use error::Result;
pub use vmm::Vmm;
