use vm_memory::GuestMemoryBackend as _;

/// Guest physical address at which the single memory region starts.
pub const GUEST_MEM_START: u64 = 0;

/// Exclusive end of the guest RAM GPA range.
pub const GUEST_RAM_END: u64 = 0xd000_0000;

/// Maximum guest memory size in bytes for the single low memslot.
pub const MAX_GUEST_MEM_SIZE: u64 = GUEST_RAM_END - GUEST_MEM_START;

/// Maximum guest memory size in MiB (`MAX_GUEST_MEM_SIZE` / 1 MiB).
pub const MAX_MEMORY_MIB: u32 = (MAX_GUEST_MEM_SIZE / (1024 * 1024)) as u32;

/// Check that `size` is a valid guest RAM size for the single low memslot.
///
/// KVM maps `[GUEST_MEM_START, GUEST_MEM_START + size)`. The exclusive end must not exceed
/// `GUEST_RAM_END`.
pub fn validate_mem_size(size: usize) -> crate::error::Result<()> {
    if size == 0 || !size.is_multiple_of(4096) {
        return Err(crate::error::Error::InvalidMemorySize(size));
    }
    let Some(end) = GUEST_MEM_START.checked_add(size as u64) else {
        return Err(crate::error::Error::InvalidMemorySize(size));
    };
    if end > GUEST_RAM_END {
        return Err(crate::error::Error::MemoryOverlapsMmio {
            size,
            max: MAX_GUEST_MEM_SIZE as usize,
            mmio_base: GUEST_RAM_END,
        });
    }
    Ok(())
}

/// Allocate anonymous guest memory and register it with KVM.
pub fn create_guest_memory(
    vm: &kvm_ioctls::VmFd,
    size: usize,
) -> crate::error::Result<vm_memory::GuestMemoryMmap<()>> {
    validate_mem_size(size)?;

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

#[cfg(test)]
mod tests {
    #[test]
    fn accepts_page_multiple_up_to_ram_end() {
        super::validate_mem_size(4096).expect("min page");
        super::validate_mem_size(super::MAX_GUEST_MEM_SIZE as usize)
            .expect("size fills [GUEST_MEM_START, GUEST_RAM_END)");
    }

    #[test]
    fn rejects_zero_and_unaligned() {
        assert!(super::validate_mem_size(0).is_err());
        assert!(super::validate_mem_size(4095).is_err());
    }

    #[test]
    fn rejects_overlap_with_mmio() {
        let over = super::MAX_GUEST_MEM_SIZE as usize + 4096;
        match super::validate_mem_size(over) {
            Err(crate::error::Error::MemoryOverlapsMmio {
                size,
                max,
                mmio_base,
            }) => {
                assert_eq!(size, over);
                assert_eq!(max, super::MAX_GUEST_MEM_SIZE as usize);
                assert_eq!(mmio_base, super::GUEST_RAM_END);
            }
            other => panic!("expected MemoryOverlapsMmio, got {other:?}"),
        }
    }

    #[test]
    fn max_memory_mib_matches_max_guest_mem_size() {
        assert_eq!(
            u64::from(super::MAX_MEMORY_MIB) * 1024 * 1024,
            super::MAX_GUEST_MEM_SIZE
        );
        assert_eq!(
            super::MAX_GUEST_MEM_SIZE,
            super::GUEST_RAM_END - super::GUEST_MEM_START
        );
    }
}
