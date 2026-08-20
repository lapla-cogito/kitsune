//! vCPU register setup for real mode, PVH, and 64-bit Linux boot.

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

/// Install host-supported CPUID with a consistent flat guest topology.
pub fn setup_cpuid(
    kvm: &kvm_ioctls::Kvm,
    vcpu: &kvm_ioctls::VcpuFd,
    vcpu_id: u8,
    num_vcpus: u8,
) -> crate::error::Result<()> {
    let cpuid = kvm
        .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
        .map_err(crate::error::Error::KvmIoctl)?;

    let mut entries = cpuid.as_slice().to_vec();
    apply_cpu_topology(&mut entries, vcpu_id, num_vcpus);

    let cpuid =
        kvm_bindings::CpuId::from_entries(&entries).map_err(|_| crate::error::Error::CpuidSetup)?;
    vcpu.set_cpuid2(&cpuid)
        .map_err(crate::error::Error::KvmIoctl)?;
    Ok(())
}

/// Bits of APIC ID that address logical processors within one package.
fn package_id_shift() -> u32 {
    u32::from(crate::config::MAX_VCPUS)
        .next_power_of_two()
        .trailing_zeros()
}

fn max_logical_processors(num_vcpus: u8) -> u32 {
    u32::from(num_vcpus.next_power_of_two())
}

fn ensure_subleaf(entries: &mut Vec<kvm_bindings::kvm_cpuid_entry2>, function: u32, index: u32) {
    if entries
        .iter()
        .any(|e| e.function == function && e.index == index)
    {
        return;
    }
    entries.push(kvm_bindings::kvm_cpuid_entry2 {
        function,
        index,
        flags: kvm_bindings::KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0; 3],
    });
}

fn apply_cpu_topology(
    entries: &mut Vec<kvm_bindings::kvm_cpuid_entry2>,
    vcpu_id: u8,
    num_vcpus: u8,
) {
    debug_assert!(num_vcpus >= 1);

    // Advertise leaf 0xB when missing so multi-vCPU guests can enumerate topology.
    if let Some(leaf0) = entries.iter_mut().find(|e| e.function == 0)
        && leaf0.eax < 0xb
    {
        leaf0.eax = 0xb;
    }
    ensure_subleaf(entries, 0xb, 0);
    ensure_subleaf(entries, 0xb, 1);

    let max_lps = max_logical_processors(num_vcpus);
    let pkg_shift = package_id_shift();
    const THREADS_PER_CORE: u32 = 1;
    const THREAD_LEVEL_SHIFT: u32 = 0;

    for entry in entries.iter_mut() {
        match entry.function {
            0x1 => {
                let brand = entry.ebx & 0xff;
                entry.ebx =
                    brand | (8 << 8) | ((max_lps & 0xff) << 16) | (u32::from(vcpu_id) << 24);
                // ECX[31] hypervisor present.
                entry.ecx |= 1 << 31;
                // EDX[28] HTT: max-IDs field is valid when >1 logical processor.
                if num_vcpus > 1 {
                    entry.edx |= 1 << 28;
                } else {
                    entry.edx &= !(1 << 28);
                }
            }
            0xb => {
                entry.flags = kvm_bindings::KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
                entry.edx = u32::from(vcpu_id);
                match entry.index {
                    // Level type 1: SMT / logical processor.
                    0 => {
                        entry.eax = THREAD_LEVEL_SHIFT & 0x1f;
                        entry.ebx = THREADS_PER_CORE & 0xffff;
                        entry.ecx = 1 << 8;
                    }
                    // Level type 2: core (all vCPUs are cores in one package).
                    1 => {
                        entry.eax = pkg_shift & 0x1f;
                        entry.ebx = u32::from(num_vcpus) & 0xffff;
                        entry.ecx = 1 | (2 << 8);
                    }
                    n => {
                        entry.eax = 0;
                        entry.ebx = 0;
                        entry.ecx = n;
                    }
                }
            }
            0x4 => {
                // Invalid cache subleaf: leave zeros.
                if entry.eax | entry.ebx | entry.ecx | entry.edx == 0 {
                    continue;
                }
                let level = (entry.eax >> 5) & 0x7;
                // EAX[25:14] = max IDs sharing this cache minus 1.
                let share = match level {
                    1 | 2 => THREADS_PER_CORE.saturating_sub(1),
                    3 => u32::from(num_vcpus.saturating_sub(1)),
                    _ => continue,
                };
                entry.eax = (entry.eax & !(0xfff << 14)) | ((share & 0xfff) << 14);
                // EAX[31:26] = max core IDs in package minus 1.
                let cores_m1 = u32::from(num_vcpus.saturating_sub(1));
                entry.eax = (entry.eax & !(0x3f << 26)) | ((cores_m1 & 0x3f) << 26);
            }
            // AMD: NC / ApicIdCoreIdSize.
            0x8000_0008 => {
                let nc = u32::from(num_vcpus.saturating_sub(1)) & 0xff;
                entry.ecx = (entry.ecx & !0xff) | nc;
                entry.ecx = (entry.ecx & !(0xf << 12)) | ((pkg_shift.min(0xf)) << 12);
            }
            // AMD extended APIC ID.
            0x8000_001e => {
                entry.eax = u32::from(vcpu_id);
                // EBX[7:0] compute unit id; [15:8] threads per CU minus 1.
                entry.ebx = u32::from(vcpu_id) | ((THREADS_PER_CORE.saturating_sub(1)) << 8);
                entry.ecx = 0;
            }
            _ => {}
        }
    }

    // Leaf 0x1F is preferred over 0xB when present: mirror our 0xB topology.
    if entries.iter().any(|e| e.function == 0x1f) {
        let topology: Vec<_> = entries
            .iter()
            .filter(|e| e.function == 0xb)
            .cloned()
            .collect();
        entries.retain(|e| e.function != 0x1f);
        for mut e in topology {
            e.function = 0x1f;
            entries.push(e);
        }
    }
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

/// Configure a vCPU for PVH (32-bit protected mode, paging disabled).
///
/// `entry` is `XEN_ELFNOTE_PHYS32_ENTRY`. `start_info` is written to `RBX`.
pub fn setup_pvh(
    vcpu: &kvm_ioctls::VcpuFd,
    mem: &vm_memory::GuestMemoryMmap<()>,
    entry: u64,
    start_info: u64,
) -> crate::error::Result<()> {
    setup_boot_msrs(vcpu)?;
    setup_sregs_pvh(vcpu, mem)?;
    setup_regs_pvh(vcpu, entry, start_info)?;
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

fn setup_sregs_pvh(
    vcpu: &kvm_ioctls::VcpuFd,
    mem: &vm_memory::GuestMemoryMmap<()>,
) -> crate::error::Result<()> {
    let mut sregs = vcpu.get_sregs().map_err(crate::error::Error::KvmIoctl)?;

    let gdt = crate::gdt::BootGdt::pvh();
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

    // Xen PVH: PE=1, paging off, no long mode.
    sregs.cr0 = X86_CR0_PE;
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.efer = 0;

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

fn setup_regs_pvh(
    vcpu: &kvm_ioctls::VcpuFd,
    entry: u64,
    start_info: u64,
) -> crate::error::Result<()> {
    let regs = kvm_bindings::kvm_regs {
        rflags: 2,
        rip: entry,
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rbx: start_info,
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

    fn blank_entry(function: u32, index: u32) -> kvm_bindings::kvm_cpuid_entry2 {
        kvm_bindings::kvm_cpuid_entry2 {
            function,
            index,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0; 3],
        }
    }

    #[test]
    fn topology_leaf1_and_0xb_for_four_vcpus() {
        let mut entries = vec![
            blank_entry(0, 0),
            blank_entry(1, 0),
            // Host only advertised SMT level; core level must be inserted.
            blank_entry(0xb, 0),
        ];
        entries[0].eax = 0x1; // max leaf below 0xB before normalize
        entries[1].ebx = 0x0000_0800;
        entries[1].edx = 0;

        super::apply_cpu_topology(&mut entries, 3, 4);

        let leaf0 = entries.iter().find(|e| e.function == 0).unwrap();
        assert!(leaf0.eax >= 0xb);

        let leaf1 = entries.iter().find(|e| e.function == 1).unwrap();
        assert_eq!((leaf1.ebx >> 24) & 0xff, 3); // APIC ID
        assert_eq!((leaf1.ebx >> 16) & 0xff, 4); // max LPs (power of two)
        assert_eq!((leaf1.ebx >> 8) & 0xff, 8); // CLFLUSH line size
        assert_ne!(leaf1.edx & (1 << 28), 0); // HTT
        assert_ne!(leaf1.ecx & (1 << 31), 0); // hypervisor

        let smt = entries
            .iter()
            .find(|e| e.function == 0xb && e.index == 0)
            .unwrap();
        assert_eq!(smt.eax & 0x1f, 0);
        assert_eq!(smt.ebx & 0xffff, 1);
        assert_eq!((smt.ecx >> 8) & 0xff, 1);
        assert_eq!(smt.edx, 3);

        let core = entries
            .iter()
            .find(|e| e.function == 0xb && e.index == 1)
            .unwrap();
        assert_eq!(core.eax & 0x1f, super::package_id_shift());
        assert_eq!(core.ebx & 0xffff, 4);
        assert_eq!((core.ecx >> 8) & 0xff, 2);
        assert_eq!(core.edx, 3);
    }

    #[test]
    fn topology_single_vcpu_clears_htt() {
        let mut entries = vec![blank_entry(0, 0), blank_entry(1, 0)];
        entries[0].eax = 0xd;
        entries[1].edx = 1 << 28;
        super::apply_cpu_topology(&mut entries, 0, 1);
        let leaf1 = entries.iter().find(|e| e.function == 1).unwrap();
        assert_eq!(leaf1.edx & (1 << 28), 0);
        assert_eq!((leaf1.ebx >> 16) & 0xff, 1);
    }

    #[test]
    fn topology_leaf_1f_mirrors_0xb() {
        let mut entries = vec![
            blank_entry(0, 0),
            blank_entry(1, 0),
            blank_entry(0xb, 0),
            blank_entry(0xb, 1),
            blank_entry(0x1f, 0),
        ];
        entries[0].eax = 0x1f;
        super::apply_cpu_topology(&mut entries, 1, 2);
        let b0 = entries
            .iter()
            .find(|e| e.function == 0xb && e.index == 0)
            .unwrap();
        let f0 = entries
            .iter()
            .find(|e| e.function == 0x1f && e.index == 0)
            .unwrap();
        assert_eq!(f0.eax, b0.eax);
        assert_eq!(f0.ebx, b0.ebx);
        assert_eq!(f0.ecx, b0.ecx);
        assert_eq!(f0.edx, b0.edx);
    }

    #[test]
    fn topology_leaf4_l3_shared_by_all_vcpus() {
        let mut entries = vec![blank_entry(0, 0), blank_entry(1, 0), blank_entry(4, 0)];
        entries[0].eax = 0x4;
        // Cache level 3 in EAX[7:5]
        entries[2].eax = 3 << 5;
        super::apply_cpu_topology(&mut entries, 0, 8);
        let l3 = entries.iter().find(|e| e.function == 4).unwrap();
        assert_eq!((l3.eax >> 14) & 0xfff, 7); // num_vcpus - 1
        assert_eq!((l3.eax >> 26) & 0x3f, 7); // cores - 1
    }
}
