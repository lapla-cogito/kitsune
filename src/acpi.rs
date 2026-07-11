//! ACPI tables for IOAPIC, serial (COM1), and optional virtio-mmio.
//!
//! Without MADT/IOAPIC the guest stays in virtual-wire mode and never
//! receives irqfd-injected GSIs, so the serial console cannot accept input.
//! COM1 is described in the DSDT for HwReducedAcpi. Virtio-mmio devices
//! (LNRO0005) are listed when a block device is present; modern kernels
//! without `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` rely on that path.

use acpi_tables::Aml as _;
use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

/// Guest-physical base for RSDP and following tables.
/// Placed in the classic BIOS ROM window so Linux finds the RSDP without
/// `acpi_rsdp=` (which many microVM kernels do not wire as an early_param).
pub const ACPI_TABLES_BASE: u64 = 0x000e_0000;

const OEM_ID: [u8; 6] = *b"KITUNE";
const OEM_TABLE_ID: [u8; 8] = *b"KITSUNE ";
const OEM_REVISION: u32 = 1;

/// Local APIC MMIO base used by KVM's in-kernel irqchip.
const LAPIC_ADDR: u32 = 0xfee0_0000;
/// I/O APIC MMIO base used by KVM's in-kernel irqchip.
const IOAPIC_ADDR: u32 = 0xfec0_0000;

/// Description of a virtio-mmio device to advertise via DSDT.
pub struct VirtioMmioAcpi {
    pub base: u32,
    pub size: u32,
    pub irq: u32,
    pub uid: u32,
}

/// Build RSDP + XSDT + FADT + MADT + DSDT and write them into guest memory.
///
/// `num_vcpus` Local APICs are advertised in the MADT (APIC IDs `0..num_vcpus`).
/// Returns the guest-physical address of the RSDP (for `acpi_rsdp=`).
pub fn install_tables(
    mem: &vm_memory::GuestMemoryMmap<()>,
    virtio_devices: &[VirtioMmioAcpi],
    num_vcpus: u8,
) -> crate::error::Result<u64> {
    if num_vcpus == 0 {
        return Err(crate::error::Error::InvalidVcpuCount(
            0,
            crate::config::MAX_VCPUS,
        ));
    }
    // Tables occupy the 128 KiB BIOS hole (0xe0000..0x100000), which is
    // backed by our single KVM memslot starting at GPA 0.
    if mem.last_addr().0 < 0x10_0000 {
        return Err(crate::error::Error::GuestMemory(
            "guest memory too small for ACPI tables".into(),
        ));
    }

    let mut cursor = ACPI_TABLES_BASE;

    // --- DSDT (AML body) -------------------------------------------------
    let mut aml = Vec::new();
    // COM1 is required under HwReducedAcpi so userspace can use the 16550
    // (kernel printk still works via the polled console path alone).
    append_com1_device(&mut aml);
    for dev in virtio_devices {
        append_virtio_mmio_device(&mut aml, dev);
    }

    let mut dsdt = acpi_tables::sdt::Sdt::new(*b"DSDT", 36, 2, OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    dsdt.append_slice(&aml);
    let dsdt_addr = cursor;
    write_bytes(mem, dsdt_addr, dsdt.as_slice())?;
    cursor += align_up(dsdt.len() as u64, 16);

    // --- FADT ------------------------------------------------------------
    // HwReducedAcpi is required for this kernel's virtio-mmio (LNRO0005) path;
    // without it the block device is not discovered.
    let fadt = acpi_tables::fadt::FADTBuilder::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION)
        .dsdt_64(dsdt_addr)
        .flag(acpi_tables::fadt::Flags::HwReducedAcpi)
        .finalize();
    let mut fadt_bytes = Vec::new();
    fadt.to_aml_bytes(&mut fadt_bytes);
    let fadt_addr = cursor;
    write_bytes(mem, fadt_addr, &fadt_bytes)?;
    cursor += align_up(fadt_bytes.len() as u64, 16);

    // --- MADT ------------------------------------------------------------
    let mut madt = acpi_tables::madt::MADT::new(
        OEM_ID,
        OEM_TABLE_ID,
        OEM_REVISION,
        acpi_tables::madt::LocalInterruptController::Address(LAPIC_ADDR),
    );
    for apic_id in 0..num_vcpus {
        madt.add_structure(acpi_tables::madt::ProcessorLocalApic::new(
            apic_id,
            apic_id,
            acpi_tables::madt::EnabledStatus::Enabled,
        ));
    }
    madt.add_structure(acpi_tables::madt::IoApic::new(0, IOAPIC_ADDR, 0));
    let mut madt_bytes = Vec::new();
    madt.to_aml_bytes(&mut madt_bytes);
    let madt_addr = cursor;
    write_bytes(mem, madt_addr, &madt_bytes)?;
    cursor += align_up(madt_bytes.len() as u64, 16);

    // --- XSDT ------------------------------------------------------------
    let mut xsdt = acpi_tables::xsdt::XSDT::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    xsdt.add_entry(fadt_addr);
    xsdt.add_entry(madt_addr);
    let mut xsdt_bytes = Vec::new();
    xsdt.to_aml_bytes(&mut xsdt_bytes);
    let xsdt_addr = cursor;
    write_bytes(mem, xsdt_addr, &xsdt_bytes)?;
    cursor += align_up(xsdt_bytes.len() as u64, 16);

    // --- RSDP ------------------------------------------------------------
    let rsdp = acpi_tables::rsdp::Rsdp::new(OEM_ID, xsdt_addr);
    let mut rsdp_bytes = Vec::new();
    rsdp.to_aml_bytes(&mut rsdp_bytes);
    let rsdp_addr = cursor;
    write_bytes(mem, rsdp_addr, &rsdp_bytes)?;

    Ok(rsdp_addr)
}

fn append_com1_device(out: &mut Vec<u8>) {
    // PNP0501 = 16550A-compatible COM port (ISA COM1 @ 0x3f8, IRQ 4).
    let hid = acpi_tables::aml::EISAName::new("PNP0501");
    // Edge, active-high, exclusive (classic ISA serial).
    let irq =
        acpi_tables::aml::Interrupt::new(true, true, false, false, crate::devices::SERIAL_IRQ);
    let io = acpi_tables::aml::IO::new(0x3f8, 0x3f8, 0, 0x8);
    let crs = acpi_tables::aml::ResourceTemplate::new(vec![&irq, &io]);

    let hid_name = acpi_tables::aml::Name::new(acpi_tables::aml::Path::new("_HID"), &hid);
    let crs_name = acpi_tables::aml::Name::new(acpi_tables::aml::Path::new("_CRS"), &crs);

    acpi_tables::aml::Device::new(
        acpi_tables::aml::Path::new("_SB_.COM1"),
        vec![&hid_name, &crs_name],
    )
    .to_aml_bytes(out);
}

fn append_virtio_mmio_device(out: &mut Vec<u8>, dev: &VirtioMmioAcpi) {
    // Each ACPI name segment must be exactly 4 characters.
    let name = format!("_SB_.V{:03X}", dev.uid);
    let path = acpi_tables::aml::Path::new(&name);

    let hid: acpi_tables::aml::AmlStr = "LNRO0005";
    let uid = dev.uid;
    let mem32 = acpi_tables::aml::Memory32Fixed::new(true, dev.base, dev.size);
    // Edge, active-high, exclusive (matches Firecracker virtio-mmio).
    // Interrupt::new(consumer, edge_triggered, active_low, shared, gsi)
    let irq = acpi_tables::aml::Interrupt::new(true, true, false, false, dev.irq);
    let crs = acpi_tables::aml::ResourceTemplate::new(vec![&mem32, &irq]);

    let hid_name = acpi_tables::aml::Name::new(acpi_tables::aml::Path::new("_HID"), &hid);
    let uid_name = acpi_tables::aml::Name::new(acpi_tables::aml::Path::new("_UID"), &uid);
    let cca_name =
        acpi_tables::aml::Name::new(acpi_tables::aml::Path::new("_CCA"), &acpi_tables::aml::ONE);
    let crs_name = acpi_tables::aml::Name::new(acpi_tables::aml::Path::new("_CRS"), &crs);

    acpi_tables::aml::Device::new(path, vec![&hid_name, &uid_name, &cca_name, &crs_name])
        .to_aml_bytes(out);
}

fn write_bytes(
    mem: &vm_memory::GuestMemoryMmap<()>,
    addr: u64,
    data: &[u8],
) -> crate::error::Result<()> {
    mem.write_slice(data, vm_memory::GuestAddress(addr))
        .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))
}

fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}
