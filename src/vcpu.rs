//! vCPU register setup for real mode and 64-bit Linux boot.

use vm_memory::Address as _;
use vm_memory::bytes::Bytes as _;

const BOOT_STACK_POINTER: u64 = crate::boot::BOOT_STACK_POINTER;
const ZERO_PAGE_START: u64 = crate::boot::ZERO_PAGE_START;

const PML4_START: u64 = 0x9000;
const PDPTE_START: u64 = 0xa000;
const PDE_START: u64 = 0xb000;

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

    // Identity-map the first 1 GiB with 2 MiB pages.
    let boot_pml4 = vm_memory::GuestAddress(PML4_START);
    let boot_pdpte = vm_memory::GuestAddress(PDPTE_START);
    let boot_pde = vm_memory::GuestAddress(PDE_START);

    mem.write_obj(boot_pdpte.raw_value() | 0x03, boot_pml4)
        .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;
    mem.write_obj(boot_pde.raw_value() | 0x03, boot_pdpte)
        .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;
    for i in 0..512u64 {
        mem.write_obj((i << 21) + 0x83, boot_pde.unchecked_add(i * 8))
            .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;
    }

    sregs.cr3 = boot_pml4.raw_value();
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PE | X86_CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;

    vcpu.set_sregs(&sregs)
        .map_err(crate::error::Error::KvmIoctl)?;
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
