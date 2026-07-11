//! Emulated devices attached to the VM.

mod serial;
mod virtio_blk;

pub(crate) use serial::SerialConsole;
pub use virtio_blk::VirtioBlock;
