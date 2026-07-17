//! Load a Linux kernel and build boot parameters (x86_64 Linux boot protocol).

use linux_loader::configurator::BootConfigurator as _;
use linux_loader::loader::KernelLoader as _;
use vm_memory::Address as _;
use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

/// High memory start / default kernel load address (1 MiB).
pub const HIMEM_START: u64 = 0x0010_0000;
/// Zero page for `boot_params`.
pub const ZERO_PAGE_START: u64 = 0x7000;
/// Kernel command line location in guest memory.
pub const CMDLINE_START: u64 = 0x0002_0000;
/// Initial boot stack pointer.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// KVM identity-map / TSS region (must not overlap guest RAM use).
pub const KVM_TSS_ADDRESS: usize = 0xfffb_d000;

const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
const KERNEL_LOADER_OTHER: u8 = 0xff;
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;
const EBDA_START: u64 = 0x0009_fc00;
const E820_RAM: u32 = 1;

/// Kernel image and optional initrd to boot.
pub struct KernelBootConfig<'a> {
    pub kernel: &'a std::path::Path,
    pub initrd: Option<&'a std::path::Path>,
    pub cmdline: &'a str,
}

/// Load kernel (+ optional initrd), write cmdline and boot_params, return entry RIP.
pub fn load_linux(
    mem: &vm_memory::GuestMemoryMmap<()>,
    config: &KernelBootConfig<'_>,
) -> crate::error::Result<u64> {
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

    let mut params = build_boot_params(mem, &kernel_load)?;
    params.hdr.cmd_line_ptr = CMDLINE_START as u32;
    params.hdr.cmdline_size = config.cmdline.len() as u32 + 1;

    if let Some(initrd_path) = config.initrd {
        let initrd = std::fs::read(initrd_path).map_err(crate::error::Error::ImageIo)?;
        let initrd_addr = place_initrd(mem, kernel_load.kernel_end, &initrd)?;
        params.hdr.ramdisk_image = initrd_addr as u32;
        params.hdr.ramdisk_size = initrd.len() as u32;
    }

    let boot_params = linux_loader::configurator::BootParams::new(
        &params,
        vm_memory::GuestAddress(ZERO_PAGE_START),
    );
    linux_loader::configurator::linux::LinuxBootConfigurator::write_bootparams(&boot_params, mem)
        .map_err(|e| crate::error::Error::BootConfigure(e.to_string()))?;

    Ok(kernel_load.kernel_load.raw_value())
}

fn build_boot_params(
    mem: &vm_memory::GuestMemoryMmap<()>,
    kernel_load: &linux_loader::loader::KernelLoaderResult,
) -> crate::error::Result<linux_loader::loader::bootparam::boot_params> {
    let mut params = linux_loader::loader::bootparam::boot_params::default();

    if let Some(hdr) = kernel_load.setup_header {
        params.hdr = hdr;
    } else {
        params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
        params.hdr.header = KERNEL_HDR_MAGIC;
        params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
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
