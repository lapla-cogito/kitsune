//! Emulated devices attached to the VM.

mod serial;
mod virtio_blk;
mod virtio_mmio;
mod virtio_net;

pub(crate) use serial::SERIAL_IRQ;
pub(crate) use serial::SerialConsole;
pub use virtio_blk::VirtioBlock;
pub use virtio_net::VirtioNet;
