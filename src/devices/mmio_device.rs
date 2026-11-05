//! Guest MMIO devices, dispatched by physical address.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MmioKind {
    Block,
    Net,
}

/// Device the vCPU loop can service without knowing its concrete type.
pub(crate) trait MmioDevice: Send + Sync {
    fn kind(&self) -> MmioKind;
    fn range(&self) -> std::ops::Range<u64>;
    fn acpi(&self) -> crate::acpi::VirtioMmioAcpi;
    fn read(&self, addr: u64, data: &mut [u8]);
    fn write(
        &self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()>;
    fn start(&self, mem: vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()>;
    fn stop(&self) -> crate::error::Result<()>;
}
