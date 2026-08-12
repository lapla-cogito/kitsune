//! vCPU register setup for real mode and 64-bit Linux boot.

use vm_memory::Address as _;
use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

const BOOT_STACK_POINTER: u64 = crate::boot::BOOT_STACK_POINTER;
const ZERO_PAGE_START: u64 = crate::boot::ZERO_PAGE_START;

const PML4_START: u64 = 0x9000;
const PDPT_START: u64 = 0xa000;
/// First page-directory table.
const PD_TABLES_START: u64 = 0xb000;

const PAGE_SIZE: u64 = 0x1000;
const HUGE_PAGE_2M: u64 = 0x20_0000;
const PD_ENTRIES: u64 = 512;

const PTE_PRESENT_WRITABLE: u64 = 0x03;
const PTE_PRESENT_WRITABLE_PS: u64 = 0x83;

const X86_CR0_PE: u64 = 0x1;
const X86_CR0_PG: u64 = 0x8000_0000;
const X86_CR4_PAE: u64 = 0x20;
const EFER_LME: u64 = 0x0000_0100;
const EFER_LMA: u64 = 0x0000_0400;

const MSR_IA32_SYSENTER_CS: u32 = 0x0000_0174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x0000_0175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x0000_0176;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;
const MSR_IA32_TSC: u32 = 0x0000_0010;
const MSR_IA32_MISC_ENABLE: u32 = 0x0000_01a0;
const MSR_IA32_MISC_ENABLE_FAST_STRING: u64 = 1;

/// Install host-supported CPUID with per-vCPU APIC ID and logical CPU count.
pub fn setup_cpuid(
    kvm: &kvm_ioctls::Kvm,
    vcpu: &kvm_ioctls::VcpuFd,
    vcpu_id: u8,
    num_vcpus: u8,
) -> crate::error::Result<()> {
    let mut cpuid = kvm
        .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
        .map_err(crate::error::Error::KvmIoctl)?;

    for entry in cpuid.as_mut_slice() {
        match entry.function {
            1 => {
                // EBX[31:24] = initial APIC ID; EBX[23:16] = logical processors.
                entry.ebx = (entry.ebx & 0x0000_ffff)
                    | (u32::from(num_vcpus) << 16)
                    | (u32::from(vcpu_id) << 24);
                if num_vcpus > 1 {
                    // EDX bit 28: HTT (multi-threaded / multi-core topology present).
                    entry.edx |= 1 << 28;
                }
            }
            0xb if entry.index == 0 => {
                // Extended topology: x2APIC ID in EDX.
                entry.edx = u32::from(vcpu_id);
            }
            _ => {}
        }
    }

    vcpu.set_cpuid2(&cpuid)
        .map_err(crate::error::Error::KvmIoctl)?;
    Ok(())
}

/// Configure a vCPU for 16-bit real mode with CS base 0 and the given entry RIP.
pub fn setup_real_mode(vcpu: &kvm_ioctls::VcpuFd, entry: u64) -> crate::error::Result<()> {
    let mut sregs = vcpu.get_sregs().map_err(crate::error::Error::KvmIoctl)?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs)
        .map_err(crate::error::Error::KvmIoctl)?;

    let mut regs = vcpu.get_regs().map_err(crate::error::Error::KvmIoctl)?;
    regs.rip = entry;
    regs.rflags = 2;
    vcpu.set_regs(&regs)
        .map_err(crate::error::Error::KvmIoctl)?;

    Ok(())
}

/// Configure a vCPU for 64-bit Linux direct boot at `entry`.
pub fn setup_long_mode(
    vcpu: &kvm_ioctls::VcpuFd,
    mem: &vm_memory::GuestMemoryMmap<()>,
    entry: u64,
) -> crate::error::Result<()> {
    setup_boot_msrs(vcpu)?;
    setup_sregs_long_mode(vcpu, mem)?;
    setup_regs_long_mode(vcpu, entry)?;
    setup_fpu(vcpu)?;
    Ok(())
}

fn setup_boot_msrs(vcpu: &kvm_ioctls::VcpuFd) -> crate::error::Result<()> {
    let entry = |index, data| kvm_bindings::kvm_msr_entry {
        index,
        data,
        ..Default::default()
    };
    let msrs = kvm_bindings::Msrs::from_entries(&[
        entry(MSR_IA32_SYSENTER_CS, 0),
        entry(MSR_IA32_SYSENTER_ESP, 0),
        entry(MSR_IA32_SYSENTER_EIP, 0),
        entry(MSR_STAR, 0),
        entry(MSR_CSTAR, 0),
        entry(MSR_KERNEL_GS_BASE, 0),
        entry(MSR_SYSCALL_MASK, 0),
        entry(MSR_LSTAR, 0),
        entry(MSR_IA32_TSC, 0),
        entry(MSR_IA32_MISC_ENABLE, MSR_IA32_MISC_ENABLE_FAST_STRING),
    ])
    .map_err(|_| crate::error::Error::MsrSetup)?;

    let written = vcpu
        .set_msrs(&msrs)
        .map_err(crate::error::Error::KvmIoctl)?;
    if written != msrs.as_fam_struct_ref().nmsrs as usize {
        return Err(crate::error::Error::MsrSetup);
    }
    Ok(())
}

fn setup_sregs_long_mode(
    vcpu: &kvm_ioctls::VcpuFd,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<()> {
    let mut sregs = vcpu.get_sregs().map_err(crate::error::Error::KvmIoctl)?;

    let gdt = crate::gdt::BootGdt::new();
    gdt.write_to_mem(mem)?;
    crate::gdt::write_idt(mem)?;

    sregs.gdt.base = crate::gdt::BOOT_GDT_OFFSET;
    sregs.gdt.limit = gdt.limit();
    sregs.idt.base = crate::gdt::BOOT_IDT_OFFSET;
    sregs.idt.limit = (std::mem::size_of::<u64>() - 1) as u16;

    let code = gdt.code_segment();
    let data = gdt.data_segment();
    let tss = gdt.tss_segment();
    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = tss;

    setup_identity_map(mem)?;

    sregs.cr3 = PML4_START;
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PE | X86_CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;

    vcpu.set_sregs(&sregs)
        .map_err(crate::error::Error::KvmIoctl)?;
    Ok(())
}

fn setup_identity_map(mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
    let ram_end = mem
        .last_addr()
        .raw_value()
        .checked_add(1)
        .ok_or_else(|| crate::error::Error::GuestMemory("guest RAM end overflow".into()))?;
    let map_end = ram_end.div_ceil(HUGE_PAGE_2M).saturating_mul(HUGE_PAGE_2M);
    let num_2mib = map_end / HUGE_PAGE_2M;
    let num_pd = num_2mib.div_ceil(PD_ENTRIES);

    if num_pd == 0 || num_pd > PD_ENTRIES {
        return Err(crate::error::Error::GuestMemory(format!(
            "cannot identity-map {ram_end:#x} bytes of guest RAM"
        )));
    }

    // Page tables must stay below the kernel cmdline and outside other boot data.
    let pt_end = PD_TABLES_START
        .checked_add(num_pd.saturating_mul(PAGE_SIZE))
        .ok_or_else(|| crate::error::Error::GuestMemory("page table range overflow".into()))?;
    if pt_end > crate::boot::CMDLINE_START {
        return Err(crate::error::Error::GuestMemory(format!(
            "page tables [{PD_TABLES_START:#x}, {pt_end:#x}) overlap cmdline at {:#x}",
            crate::boot::CMDLINE_START
        )));
    }

    let pml4 = vm_memory::GuestAddress(PML4_START);
    let pdpt = vm_memory::GuestAddress(PDPT_START);

    // Single PML4 entry covering the low 512 GiB via one PDPT.
    mem.write_obj(pdpt.raw_value() | PTE_PRESENT_WRITABLE, pml4)
        .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;

    for pd_idx in 0..num_pd {
        let pd_gpa = PD_TABLES_START + pd_idx * PAGE_SIZE;
        mem.write_obj(
            pd_gpa | PTE_PRESENT_WRITABLE,
            pdpt.unchecked_add(pd_idx * 8),
        )
        .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;

        let pd = vm_memory::GuestAddress(pd_gpa);
        let base_2mib = pd_idx * PD_ENTRIES;
        for i in 0..PD_ENTRIES {
            let global = base_2mib + i;
            let entry = if global < num_2mib {
                (global * HUGE_PAGE_2M) | PTE_PRESENT_WRITABLE_PS
            } else {
                0
            };
            mem.write_obj(entry, pd.unchecked_add(i * 8))
                .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;
        }
    }

    for pd_idx in num_pd..PD_ENTRIES {
        mem.write_obj(0u64, pdpt.unchecked_add(pd_idx * 8))
            .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;
    }

    Ok(())
}

fn setup_regs_long_mode(vcpu: &kvm_ioctls::VcpuFd, entry: u64) -> crate::error::Result<()> {
    let regs = kvm_bindings::kvm_regs {
        rflags: 2,
        rip: entry,
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rsi: ZERO_PAGE_START,
        ..Default::default()
    };
    vcpu.set_regs(&regs)
        .map_err(crate::error::Error::KvmIoctl)?;
    Ok(())
}

fn setup_fpu(vcpu: &kvm_ioctls::VcpuFd) -> crate::error::Result<()> {
    let fpu = kvm_bindings::kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu).map_err(crate::error::Error::KvmIoctl)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guest_mem(size: usize) -> vm_memory::GuestMemoryMmap<()> {
        vm_memory::GuestMemoryMmap::<()>::from_ranges(&[(vm_memory::GuestAddress(0), size)])
            .expect("guest memory")
    }

    fn identity_map_end(ram_size: u64) -> u64 {
        ram_size.div_ceil(HUGE_PAGE_2M).saturating_mul(HUGE_PAGE_2M)
    }

    fn identity_map_pd_count(ram_size: u64) -> u64 {
        let num_2mib = identity_map_end(ram_size) / HUGE_PAGE_2M;
        num_2mib.div_ceil(PD_ENTRIES)
    }

    #[test]
    fn map_sizing_rounds_up_to_2mib_and_pd_tables() {
        assert_eq!(identity_map_end(256 * 1024 * 1024), 256 * 1024 * 1024);
        assert_eq!(identity_map_pd_count(256 * 1024 * 1024), 1);

        assert_eq!(identity_map_end(1024 * 1024 * 1024), 1024 * 1024 * 1024);
        assert_eq!(identity_map_pd_count(1024 * 1024 * 1024), 1);

        // Just over 1 GiB needs a second page directory.
        let over = 1024 * 1024 * 1024 + super::HUGE_PAGE_2M;
        assert_eq!(identity_map_pd_count(over), 2);

        // Max kitsune RAM (3.25 GiB) needs four PDs.
        assert_eq!(identity_map_pd_count(crate::memory::MAX_GUEST_MEM_SIZE), 4);
    }

    #[test]
    fn identity_map_covers_past_1gib() {
        let size = 2 * 1024 * 1024 * 1024; // 2 GiB
        let mem = guest_mem(size);
        super::setup_identity_map(&mem).expect("map");

        let e0: u64 = mem
            .read_obj(vm_memory::GuestAddress(super::PDPT_START))
            .unwrap();
        let e1: u64 = mem
            .read_obj(vm_memory::GuestAddress(super::PDPT_START + 8))
            .unwrap();
        let e2: u64 = mem
            .read_obj(vm_memory::GuestAddress(super::PDPT_START + 16))
            .unwrap();
        assert_ne!(e0 & 1, 0);
        assert_ne!(e1 & 1, 0);
        assert_eq!(e2, 0);

        // First 2 MiB of the second GiB: PDE entry at global index 512.
        let pd1 = super::PD_TABLES_START + super::PAGE_SIZE;
        let pde: u64 = mem.read_obj(vm_memory::GuestAddress(pd1)).unwrap();
        assert_eq!(
            pde & super::PTE_PRESENT_WRITABLE_PS,
            super::PTE_PRESENT_WRITABLE_PS
        );
        assert_eq!(pde & !0xfff, 1 << 30); // GPA 1 GiB
    }

    #[test]
    fn identity_map_small_ram_still_works() {
        let mem = guest_mem(64 * 1024 * 1024);
        super::setup_identity_map(&mem).expect("map");
        let pde0: u64 = mem
            .read_obj(vm_memory::GuestAddress(super::PD_TABLES_START))
            .unwrap();
        assert_eq!(pde0, super::PTE_PRESENT_WRITABLE_PS); // GPA 0, PS|RW|P

        let pde31: u64 = mem
            .read_obj(vm_memory::GuestAddress(super::PD_TABLES_START + 31 * 8))
            .unwrap();
        assert_ne!(pde31 & 1, 0);
        assert_eq!(pde31 & !0xfff, 31 * super::HUGE_PAGE_2M);

        let pde32: u64 = mem
            .read_obj(vm_memory::GuestAddress(super::PD_TABLES_START + 32 * 8))
            .unwrap();
        assert_eq!(pde32, 0);
    }

    #[test]
    fn page_tables_fit_below_cmdline() {
        let num_pd = identity_map_pd_count(crate::memory::MAX_GUEST_MEM_SIZE);
        let pt_end = super::PD_TABLES_START + num_pd * super::PAGE_SIZE;
        assert!(pt_end <= crate::boot::CMDLINE_START);
    }
}
