use vm_memory::GuestMemoryBackend as _;

/// Guest physical address at which the single memory region starts.
pub const GUEST_MEM_START: u64 = 0;

/// Allocate anonymous guest memory and register it with KVM.
pub fn create_guest_memory(
    vm: &kvm_ioctls::VmFd,
    size: usize,
) -> crate::error::Result<vm_memory::GuestMemoryMmap<()>> {
    let guest_addr = vm_memory::GuestAddress(GUEST_MEM_START);
    let guest_mem = vm_memory::GuestMemoryMmap::<()>::from_ranges(&[(guest_addr, size)])
        .map_err(|e| crate::error::Error::GuestMemory(e.to_string()))?;

    let host_addr = guest_mem
        .get_host_address(guest_addr)
        .map_err(|e| crate::error::Error::GuestMemory(e.to_string()))?;

    let region = kvm_bindings::kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: GUEST_MEM_START,
        memory_size: size as u64,
        userspace_addr: host_addr as u64,
        flags: 0,
    };

    // SAFETY: `guest_mem` owns the mapping for the lifetime of the VM and is
    // not relocated; the region covers the full mapping starting at host_addr.
    unsafe {
        vm.set_user_memory_region(region)
            .map_err(crate::error::Error::KvmIoctl)?;
    }

    Ok(guest_mem)
}
