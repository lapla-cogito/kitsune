//! Emulated devices attached to the VM.

mod legacy_io;
mod mmio_device;
mod pio_bus;
mod serial;
mod virtio_blk;
mod virtio_dev;
mod virtio_mmio;
mod virtio_net;
mod virtio_vsock;
mod vsock_mux;

pub(crate) use legacy_io::PowerAction;
pub(crate) use legacy_io::SLEEP_CONTROL_PORT;
pub(crate) use legacy_io::SLEEP_STATUS_PORT;
pub(crate) use legacy_io::SLEEP_TYP_S5;
pub(crate) use mmio_device::MmioDevice;
pub(crate) use mmio_device::MmioKind;
pub(crate) use pio_bus::PioBus;
pub(crate) use serial::SERIAL_IRQ;
pub(crate) use serial::StdinWorker;
pub use virtio_blk::VirtioBlock;
pub use virtio_net::VirtioNet;
pub use virtio_vsock::VirtioVsock;
