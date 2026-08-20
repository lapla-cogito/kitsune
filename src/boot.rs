//! Load a Linux kernel and configure the matching x86_64 boot protocol.

use linux_loader::configurator::BootConfigurator as _;
use linux_loader::loader::KernelLoader as _;
use vm_memory::Address as _;
use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

/// High memory start / default kernel load address (1 MiB).
pub const HIMEM_START: u64 = 0x0010_0000;
/// Zero page for Linux `boot_params`.
pub const ZERO_PAGE_START: u64 = 0x7000;
/// PVH `hvm_start_info`.
pub const PVH_INFO_START: u64 = 0x6000;
/// PVH memory map (`hvm_memmap_table_entry` array).
pub const PVH_MEMMAP_START: u64 = 0x6100;
/// PVH module list (initrd).
pub const PVH_MODLIST_START: u64 = 0x6200;
/// Kernel command line location in guest memory.
pub const CMDLINE_START: u64 = 0x0002_0000;
/// Initial boot stack pointer.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// KVM identity-map / TSS region (must not overlap guest RAM use).
pub const KVM_TSS_ADDRESS: usize = 0xfffb_d000;

/// 64-bit Linux boot protocol entry offset into a loaded bzImage.
const BZIMAGE_64BIT_ENTRY_OFFSET: u64 = 0x200;
/// x86_64 `__START_KERNEL_map`; ELF `e_entry` may be in this VA range.
const START_KERNEL_MAP: u64 = 0xffff_ffff_8000_0000;

const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
const KERNEL_LOADER_OTHER: u8 = 0xff;
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;
/// Boot protocol 2.12: `cmdline_size` (2.06) and `xloadflags` (2.12).
const KERNEL_BOOT_PROTOCOL: u16 = 0x020c;
const EBDA_START: u64 = 0x0009_fc00;
const E820_RAM: u32 = 1;

const _: () = assert!(PVH_INFO_START + 56 <= PVH_MEMMAP_START);
const _: () = assert!(PVH_MEMMAP_START + 256 <= PVH_MODLIST_START);
const _: () = assert!(PVH_MODLIST_START + 32 <= ZERO_PAGE_START);

/// Kernel image and optional initrd to boot.
pub struct KernelBootConfig<'a> {
    pub kernel: &'a std::path::Path,
    pub initrd: Option<&'a std::path::Path>,
    pub cmdline: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelEntry {
    Linux64 { rip: u64 },
    Pvh { rip: u64, start_info: u64 },
}

pub fn load_linux(
    mem: &vm_memory::GuestMemoryMmap<()>,
    config: &KernelBootConfig<'_>,
    rsdp: u64,
) -> crate::error::Result<KernelEntry> {
    let mut kernel_file =
        std::fs::File::open(config.kernel).map_err(crate::error::Error::ImageIo)?;

    let kernel_load = match linux_loader::loader::elf::Elf::load(
        mem,
        None,
        &mut kernel_file,
        Some(vm_memory::GuestAddress(HIMEM_START)),
    ) {
        Ok(result) => result,
        Err(linux_loader::loader::Error::Elf(
            linux_loader::loader::elf::Error::InvalidElfMagicNumber,
        )) => {
            // Rewind and try bzImage.
            use std::io::Seek as _;
            kernel_file
                .seek(std::io::SeekFrom::Start(0))
                .map_err(crate::error::Error::ImageIo)?;
            linux_loader::loader::bzimage::BzImage::load(
                mem,
                None,
                &mut kernel_file,
                Some(vm_memory::GuestAddress(HIMEM_START)),
            )
            .map_err(|e| crate::error::Error::KernelLoad(e.to_string()))?
        }
        Err(e) => return Err(crate::error::Error::KernelLoad(e.to_string())),
    };

    let mut cmdline = linux_loader::cmdline::Cmdline::new(4096)
        .map_err(|e| crate::error::Error::Cmdline(e.to_string()))?;
    cmdline
        .insert_str(config.cmdline)
        .map_err(|e| crate::error::Error::Cmdline(e.to_string()))?;

    linux_loader::loader::load_cmdline(mem, vm_memory::GuestAddress(CMDLINE_START), &cmdline)
        .map_err(|e| crate::error::Error::KernelLoad(e.to_string()))?;

    let initrd = if let Some(initrd_path) = config.initrd {
        let bytes = std::fs::read(initrd_path).map_err(crate::error::Error::ImageIo)?;
        let addr = place_initrd(mem, kernel_load.kernel_end, &bytes)?;
        Some((addr, bytes.len() as u64))
    } else {
        None
    };

    if let linux_loader::loader::elf::PvhBootCapability::PvhEntryPresent(pvh_entry) =
        kernel_load.pvh_boot_cap
    {
        write_pvh_info(mem, rsdp, initrd)?;
        return Ok(KernelEntry::Pvh {
            rip: pvh_entry.raw_value(),
            start_info: PVH_INFO_START,
        });
    }

    let mut params = build_boot_params(mem, &kernel_load)?;
    params.hdr.cmd_line_ptr = CMDLINE_START as u32;
    params.hdr.cmdline_size = config.cmdline.len() as u32 + 1;
    if let Some((addr, len)) = initrd {
        params.hdr.ramdisk_image = addr as u32;
        params.hdr.ramdisk_size = len as u32;
    }

    let boot_params = linux_loader::configurator::BootParams::new(
        &params,
        vm_memory::GuestAddress(ZERO_PAGE_START),
    );
    linux_loader::configurator::linux::LinuxBootConfigurator::write_bootparams(&boot_params, mem)
        .map_err(|e| crate::error::Error::BootConfigure(e.to_string()))?;

    Ok(KernelEntry::Linux64 {
        rip: linux64_entry(&kernel_load)?,
    })
}

fn linux64_entry(
    kernel_load: &linux_loader::loader::KernelLoaderResult,
) -> crate::error::Result<u64> {
    let base = kernel_load.kernel_load.raw_value();
    if kernel_load.setup_header.is_some() {
        // Documentation/arch/x86/boot.rst: 64-bit entry is load address + 0x200.
        base.checked_add(BZIMAGE_64BIT_ENTRY_OFFSET)
            .ok_or_else(|| crate::error::Error::KernelLoad("bzImage 64-bit entry overflow".into()))
    } else {
        Ok(physical_entry(base))
    }
}

fn physical_entry(entry: u64) -> u64 {
    if entry >= START_KERNEL_MAP {
        entry - START_KERNEL_MAP
    } else {
        entry
    }
}

fn write_pvh_info(
    mem: &vm_memory::GuestMemoryMmap<()>,
    rsdp: u64,
    initrd: Option<(u64, u64)>,
) -> crate::error::Result<()> {
    use linux_loader::loader::elf::start_info::XEN_HVM_START_MAGIC_VALUE;
    use linux_loader::loader::elf::start_info::hvm_memmap_table_entry;
    use linux_loader::loader::elf::start_info::hvm_modlist_entry;
    use linux_loader::loader::elf::start_info::hvm_start_info;

    let last = mem.last_addr().raw_value();
    if last < HIMEM_START {
        return Err(crate::error::Error::GuestMemory(
            "guest memory is smaller than high memory start".into(),
        ));
    }

    let memmap = [
        hvm_memmap_table_entry {
            addr: 0,
            size: EBDA_START,
            type_: E820_RAM,
            reserved: 0,
        },
        hvm_memmap_table_entry {
            addr: HIMEM_START,
            size: last - HIMEM_START + 1,
            type_: E820_RAM,
            reserved: 0,
        },
    ];

    let (nr_modules, modlist_paddr, modules) = match initrd {
        Some((paddr, size)) => (
            1,
            PVH_MODLIST_START,
            Some([hvm_modlist_entry {
                paddr,
                size,
                cmdline_paddr: 0,
                reserved: 0,
            }]),
        ),
        None => (0, 0, None),
    };

    let start_info = hvm_start_info {
        magic: XEN_HVM_START_MAGIC_VALUE,
        version: 1,
        flags: 0,
        nr_modules,
        modlist_paddr,
        cmdline_paddr: CMDLINE_START,
        rsdp_paddr: rsdp,
        memmap_paddr: PVH_MEMMAP_START,
        memmap_entries: memmap.len() as u32,
        reserved: 0,
    };

    let mut boot_params = linux_loader::configurator::BootParams::new(
        &start_info,
        vm_memory::GuestAddress(PVH_INFO_START),
    );
    boot_params.set_sections(&memmap, vm_memory::GuestAddress(PVH_MEMMAP_START));
    if let Some(modules) = modules.as_ref() {
        boot_params.set_modules(modules, vm_memory::GuestAddress(PVH_MODLIST_START));
    }
    linux_loader::configurator::pvh::PvhBootConfigurator::write_bootparams(&boot_params, mem)
        .map_err(|e| crate::error::Error::BootConfigure(e.to_string()))
}

fn build_boot_params(
    mem: &vm_memory::GuestMemoryMmap<()>,
    kernel_load: &linux_loader::loader::KernelLoaderResult,
) -> crate::error::Result<linux_loader::loader::bootparam::boot_params> {
    let mut params = linux_loader::loader::bootparam::boot_params::default();

    if let Some(hdr) = kernel_load.setup_header {
        params.hdr = hdr;
    } else {
        fill_elf_setup_header(&mut params.hdr);
    }
    if params.hdr.type_of_loader == 0 {
        params.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    }

    add_e820_entry(&mut params, 0, EBDA_START, E820_RAM)?;

    let last = mem.last_addr().raw_value();
    let himem = HIMEM_START;
    if last < himem {
        return Err(crate::error::Error::GuestMemory(
            "guest memory is smaller than high memory start".into(),
        ));
    }
    add_e820_entry(&mut params, himem, last - himem + 1, E820_RAM)?;

    Ok(params)
}

fn fill_elf_setup_header(hdr: &mut linux_loader::loader::bootparam::setup_header) {
    hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    hdr.header = KERNEL_HDR_MAGIC;
    hdr.version = KERNEL_BOOT_PROTOCOL;
    hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
    hdr.type_of_loader = KERNEL_LOADER_OTHER;
    hdr.loadflags = linux_loader::loader::bootparam::LOADED_HIGH;
    hdr.xloadflags = linux_loader::loader::bootparam::XLF_KERNEL_64;
}

fn add_e820_entry(
    params: &mut linux_loader::loader::bootparam::boot_params,
    addr: u64,
    size: u64,
    mem_type: u32,
) -> crate::error::Result<()> {
    let idx = params.e820_entries as usize;
    if idx >= params.e820_table.len() {
        return Err(crate::error::Error::BootConfigure(
            "too many e820 entries".into(),
        ));
    }
    params.e820_table[idx].addr = addr;
    params.e820_table[idx].size = size;
    params.e820_table[idx].r#type = mem_type;
    params.e820_entries += 1;
    Ok(())
}

fn place_initrd(
    mem: &vm_memory::GuestMemoryMmap<()>,
    kernel_end: u64,
    initrd: &[u8],
) -> crate::error::Result<u64> {
    let last = mem.last_addr().raw_value();
    let len = initrd.len() as u64;
    if len == 0 {
        return Err(crate::error::Error::KernelLoad("empty initrd".into()));
    }
    // Align down so the initrd ends at the last guest byte.
    let end = last + 1;
    let addr = (end.saturating_sub(len)) & !0xfff;
    let Some(initrd_end) = addr.checked_add(len) else {
        return Err(crate::error::Error::ImageDoesNotFit {
            load_addr: addr,
            len: initrd.len(),
        });
    };
    if addr < kernel_end || initrd_end > end {
        return Err(crate::error::Error::ImageDoesNotFit {
            load_addr: addr,
            len: initrd.len(),
        });
    }
    if addr > u64::from(u32::MAX) {
        return Err(crate::error::Error::KernelLoad(
            "initrd address does not fit in 32-bit boot_params".into(),
        ));
    }
    mem.write_slice(initrd, vm_memory::GuestAddress(addr))
        .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use linux_loader::loader::KernelLoaderResult;
    use vm_memory::GuestAddress;
    use vm_memory::bytes::Bytes as _;

    fn guest_mem(size: usize) -> vm_memory::GuestMemoryMmap<()> {
        vm_memory::GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), size)]).unwrap()
    }

    #[test]
    fn bzimage_linux64_entry_is_load_plus_0x200() {
        let load = KernelLoaderResult {
            kernel_load: GuestAddress(0x0010_0000),
            setup_header: Some(linux_loader::loader::bootparam::setup_header {
                header: super::KERNEL_HDR_MAGIC,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(super::linux64_entry(&load).unwrap(), 0x0010_0200);
    }

    #[test]
    fn elf_linux64_entry_converts_kernel_va() {
        let load = KernelLoaderResult {
            kernel_load: GuestAddress(0xffff_ffff_8235_3eb0),
            setup_header: None,
            ..Default::default()
        };
        assert_eq!(super::linux64_entry(&load).unwrap(), 0x0235_3eb0);
    }

    #[test]
    fn elf_linux64_entry_keeps_physical_address() {
        let load = KernelLoaderResult {
            kernel_load: GuestAddress(0x0235_3eb0),
            ..Default::default()
        };
        assert_eq!(super::linux64_entry(&load).unwrap(), 0x0235_3eb0);
    }

    #[test]
    fn elf_setup_header_advertises_64bit_protocol() {
        let mut hdr = linux_loader::loader::bootparam::setup_header::default();
        super::fill_elf_setup_header(&mut hdr);
        let header = hdr.header;
        let boot_flag = hdr.boot_flag;
        let version = hdr.version;
        let loadflags = hdr.loadflags;
        let xloadflags = hdr.xloadflags;
        let type_of_loader = hdr.type_of_loader;
        assert_eq!(header, super::KERNEL_HDR_MAGIC);
        assert_eq!(boot_flag, super::KERNEL_BOOT_FLAG_MAGIC);
        assert_eq!(version, super::KERNEL_BOOT_PROTOCOL);
        assert_eq!(loadflags, linux_loader::loader::bootparam::LOADED_HIGH);
        assert_eq!(xloadflags, linux_loader::loader::bootparam::XLF_KERNEL_64);
        assert_eq!(type_of_loader, super::KERNEL_LOADER_OTHER);
    }

    #[test]
    fn pvh_start_info_and_memmap_are_written() {
        let mem = guest_mem(32 * 1024 * 1024);
        let rsdp = 0xe0200;
        super::write_pvh_info(&mem, rsdp, Some((0x0100_0000, 0x1000))).unwrap();

        let info: linux_loader::loader::elf::start_info::hvm_start_info =
            mem.read_obj(GuestAddress(super::PVH_INFO_START)).unwrap();
        assert_eq!(
            info.magic,
            linux_loader::loader::elf::start_info::XEN_HVM_START_MAGIC_VALUE
        );
        assert_eq!(info.version, 1);
        assert_eq!(info.cmdline_paddr, super::CMDLINE_START);
        assert_eq!(info.rsdp_paddr, rsdp);
        assert_eq!(info.memmap_paddr, super::PVH_MEMMAP_START);
        assert_eq!(info.memmap_entries, 2);
        assert_eq!(info.nr_modules, 1);
        assert_eq!(info.modlist_paddr, super::PVH_MODLIST_START);

        let entry0: linux_loader::loader::elf::start_info::hvm_memmap_table_entry =
            mem.read_obj(GuestAddress(super::PVH_MEMMAP_START)).unwrap();
        assert_eq!(entry0.addr, 0);
        assert_eq!(entry0.size, super::EBDA_START);
        assert_eq!(entry0.type_, super::E820_RAM);

        let module: linux_loader::loader::elf::start_info::hvm_modlist_entry = mem
            .read_obj(GuestAddress(super::PVH_MODLIST_START))
            .unwrap();
        assert_eq!(module.paddr, 0x0100_0000);
        assert_eq!(module.size, 0x1000);
    }

    #[test]
    fn sample_vmlinux_selects_pvh() {
        let path = std::path::Path::new("resources/kernels/vmlinux");
        if !path.exists() {
            return;
        }
        let mem = guest_mem(256 * 1024 * 1024);
        let entry = super::load_linux(
            &mem,
            &super::KernelBootConfig {
                kernel: path,
                initrd: None,
                cmdline: "console=ttyS0",
            },
            0xe0200,
        )
        .expect("load vmlinux");
        match entry {
            super::KernelEntry::Pvh { rip, start_info } => {
                assert_eq!(rip, 0x0235_3a90);
                assert_eq!(start_info, super::PVH_INFO_START);
            }
            other => panic!("expected PVH entry, got {other:?}"),
        }
    }
}
