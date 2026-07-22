//! Emulated devices attached to the VM.

mod legacy_io;
mod serial;
mod virtio_blk;
mod virtio_mmio;
mod virtio_net;

pub(crate) use legacy_io::LegacyIo;
pub(crate) use legacy_io::PowerAction;
pub(crate) use serial::SERIAL_IRQ;
pub(crate) use serial::SerialConsole;
pub(crate) use serial::StdinWorker;
pub use virtio_blk::VirtioBlock;
pub use virtio_net::VirtioNet;
