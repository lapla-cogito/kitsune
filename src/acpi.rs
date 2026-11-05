//! ACPI tables for IOAPIC, serial (COM1), S5 poweroff, and optional virtio-mmio.
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
    append_s5(&mut aml);
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
    let mut fadt = acpi_tables::fadt::FADTBuilder::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION)
        .dsdt_64(dsdt_addr)
        .flag(acpi_tables::fadt::Flags::HwReducedAcpi);
    // Byte-wide SystemIO registers. Linux uses these only when HW_REDUCED is set.
    fadt.sleep_control_reg = sleep_gas(crate::devices::SLEEP_CONTROL_PORT);
    fadt.sleep_status_reg = sleep_gas(crate::devices::SLEEP_STATUS_PORT);
    let fadt = fadt.finalize();
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

fn sleep_gas(port: u16) -> acpi_tables::gas::GAS {
    acpi_tables::gas::GAS::new(
        acpi_tables::gas::AddressSpace::SystemIo,
        8,
        0,
        acpi_tables::gas::AccessSize::ByteAccess,
        u64::from(port),
    )
}

fn append_s5(out: &mut Vec<u8>) {
    // Nameseg `_S5_` is the ACPI name `\_S5`. Two integers: SLP_TYPa and SLP_TYPb.
    let typ_a = crate::devices::SLEEP_TYP_S5;
    let typ_b = crate::devices::SLEEP_TYP_S5;
    let package = acpi_tables::aml::Package::new(vec![&typ_a, &typ_b]);
    acpi_tables::aml::Name::new(acpi_tables::aml::Path::new("\\_S5_"), &package).to_aml_bytes(out);
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

#[cfg(test)]
mod tests {
    use vm_memory::bytes::Bytes as _;

    fn guest_mem(size: usize) -> vm_memory::GuestMemoryMmap<()> {
        vm_memory::GuestMemoryMmap::<()>::from_ranges(&[(vm_memory::GuestAddress(0), size)])
            .expect("guest memory")
    }

    #[test]
    fn install_tables_writes_rsdp_signature() {
        let mem = guest_mem(2 * 1024 * 1024);
        let rsdp = super::install_tables(&mem, &[], 1).expect("acpi");
        let mut sig = [0u8; 8];
        mem.read_slice(&mut sig, vm_memory::GuestAddress(rsdp))
            .expect("read rsdp");
        assert_eq!(&sig, b"RSD PTR ");
    }

    #[test]
    fn madt_grows_with_vcpus() {
        let mem = guest_mem(2 * 1024 * 1024);
        // One LAPIC vs many: more MADT structures → larger table blob after DSDT/FADT.
        // Compare total occupied range by scanning for non-zero growth past ACPI base.
        let _ = super::install_tables(&mem, &[], 1).expect("1 cpu");
        let mut buf1 = vec![0u8; 4096];
        mem.read_slice(&mut buf1, vm_memory::GuestAddress(super::ACPI_TABLES_BASE))
            .expect("read");

        let mem2 = guest_mem(2 * 1024 * 1024);
        let _ = super::install_tables(&mem2, &[], 4).expect("4 cpu");
        let mut buf4 = vec![0u8; 4096];
        mem2.read_slice(&mut buf4, vm_memory::GuestAddress(super::ACPI_TABLES_BASE))
            .expect("read");

        assert_ne!(buf1, buf4);
        // MADT Local APIC entry is 8 bytes each; 3 extra CPUs should enlarge the image.
        let nz1 = buf1.iter().filter(|b| **b != 0).count();
        let nz4 = buf4.iter().filter(|b| **b != 0).count();
        assert!(nz4 > nz1);
    }

    #[test]
    fn rejects_zero_vcpus() {
        let mem = guest_mem(2 * 1024 * 1024);
        assert!(super::install_tables(&mem, &[], 0).is_err());
    }

    #[test]
    fn virtio_device_changes_dsdt() {
        let mem = guest_mem(2 * 1024 * 1024);
        let _ = super::install_tables(&mem, &[], 1).expect("no virtio");
        let mut a = vec![0u8; 512];
        mem.read_slice(&mut a, vm_memory::GuestAddress(super::ACPI_TABLES_BASE))
            .unwrap();

        let mem = guest_mem(2 * 1024 * 1024);
        let devs = [super::VirtioMmioAcpi {
            base: 0xd000_0000,
            size: 0x1000,
            irq: 5,
            uid: 0,
        }];
        let _ = super::install_tables(&mem, &devs, 1).expect("virtio");
        let mut b = vec![0u8; 512];
        mem.read_slice(&mut b, vm_memory::GuestAddress(super::ACPI_TABLES_BASE))
            .unwrap();
        assert_ne!(a, b);
    }

    /// FADT offsets from the ACPI 6.x layout (`sleep_control` at `0xF4`).
    const FADT_SLEEP_CONTROL_OFFSET: usize = 0xf4;
    const FADT_SLEEP_STATUS_OFFSET: usize = 0x100;

    fn gas_bytes(port: u16) -> [u8; 12] {
        let mut gas = [0u8; 12];
        gas[0] = 1; // SystemIO
        gas[1] = 8; // bit width
        gas[3] = 1; // byte access
        let port = port.to_le_bytes();
        gas[4] = port[0];
        gas[5] = port[1];
        gas
    }

    #[test]
    fn fadt_advertises_s5_sleep_registers() {
        let mem = guest_mem(2 * 1024 * 1024);
        super::install_tables(&mem, &[], 1).expect("acpi");
        let mut buf = vec![0u8; 8192];
        mem.read_slice(&mut buf, vm_memory::GuestAddress(super::ACPI_TABLES_BASE))
            .expect("read");

        assert!(
            buf.windows(4).any(|w| w == b"_S5_"),
            "DSDT is missing \\_S5"
        );

        let facp = buf
            .windows(4)
            .position(|w| w == b"FACP")
            .expect("missing FADT");
        let length = u32::from_le_bytes(buf[facp + 4..facp + 8].try_into().unwrap()) as usize;
        // ACPI 5.0+ sleep registers start at FADT revision 5 (length >= 0x10C).
        assert!(
            length >= 0x10c,
            "FADT length {length:#x} drops sleep registers"
        );
        assert!(facp + length <= buf.len());

        let fadt = &buf[facp..facp + length];
        let sum = fadt.iter().fold(0u8, |acc, byte| acc.wrapping_add(*byte));
        assert_eq!(sum, 0, "FADT checksum");

        let ctrl = gas_bytes(crate::devices::SLEEP_CONTROL_PORT);
        let status = gas_bytes(crate::devices::SLEEP_STATUS_PORT);
        assert_eq!(
            &fadt[FADT_SLEEP_CONTROL_OFFSET..FADT_SLEEP_CONTROL_OFFSET + 12],
            &ctrl
        );
        assert_eq!(
            &fadt[FADT_SLEEP_STATUS_OFFSET..FADT_SLEEP_STATUS_OFFSET + 12],
            &status
        );
    }
}
