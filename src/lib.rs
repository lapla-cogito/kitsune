//! kitsune: a KVM-based virtual machine monitor.

mod config;
mod devices;
mod error;
mod memory;
mod vcpu;
mod vmm;

pub use config::VmmConfig;
pub use error::{Error, Result};
pub use vmm::Vmm;
