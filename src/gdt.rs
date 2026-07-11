//! Minimal GDT helpers for 64-bit Linux boot.

use vm_memory::Address as _;
use vm_memory::bytes::Bytes as _;

/// Guest address of the boot GDT.
pub const BOOT_GDT_OFFSET: u64 = 0x500;
/// Guest address of the boot IDT.
pub const BOOT_IDT_OFFSET: u64 = 0x520;

#[derive(Copy, Clone, Debug, Default)]
#[repr(transparent)]
struct SegmentDescriptor(u64);

// SAFETY: transparent wrapper over u64 with no padding.
unsafe impl vm_memory::ByteValued for SegmentDescriptor {}

impl SegmentDescriptor {
    fn from(flags: u16, base: u32, limit: u32) -> Self {
        Self(
            ((u64::from(base) & 0xff00_0000) << (56 - 24))
                | ((u64::from(flags) & 0x0000_f0ff) << 40)
                | ((u64::from(limit) & 0x000f_0000) << (48 - 16))
                | ((u64::from(base) & 0x00ff_ffff) << 16)
                | (u64::from(limit) & 0x0000_ffff),
        )
    }

    fn base(self) -> u64 {
        ((self.0 & 0xff00_0000_0000_0000) >> 32)
            | ((self.0 & 0x0000_00ff_0000_0000) >> 16)
            | ((self.0 & 0x0000_0000_ffff_0000) >> 16)
    }

    fn limit(self) -> u32 {
        (((self.0 & 0x000f_0000_0000_0000) >> 32) | (self.0 & 0x0000_0000_0000_ffff)) as u32
    }

    fn g(self) -> u8 {
        ((self.0 & 0x0080_0000_0000_0000) >> 55) as u8
    }

    fn db(self) -> u8 {
        ((self.0 & 0x0040_0000_0000_0000) >> 54) as u8
    }

    fn l(self) -> u8 {
        ((self.0 & 0x0020_0000_0000_0000) >> 53) as u8
    }

    fn avl(self) -> u8 {
        ((self.0 & 0x0010_0000_0000_0000) >> 52) as u8
    }

    fn p(self) -> u8 {
        ((self.0 & 0x0000_8000_0000_0000) >> 47) as u8
    }

    fn dpl(self) -> u8 {
        ((self.0 & 0x0000_6000_0000_0000) >> 45) as u8
    }

    fn s(self) -> u8 {
        ((self.0 & 0x0000_1000_0000_0000) >> 44) as u8
    }

    fn segment_type(self) -> u8 {
        ((self.0 & 0x0000_0f00_0000_0000) >> 40) as u8
    }

    fn to_kvm_segment(self, table_index: usize) -> kvm_bindings::kvm_segment {
        kvm_bindings::kvm_segment {
            base: self.base(),
            limit: self.limit(),
            selector: (table_index * 8) as u16,
            type_: self.segment_type(),
            present: self.p(),
            dpl: self.dpl(),
            db: self.db(),
            s: self.s(),
            l: self.l(),
            g: self.g(),
            avl: self.avl(),
            padding: 0,
            unusable: u8::from(self.p() == 0),
        }
    }
}

/// NULL, 64-bit code, data, and TSS descriptors used at boot.
pub struct BootGdt {
    entries: [SegmentDescriptor; 4],
}

impl BootGdt {
    pub fn new() -> Self {
        Self {
            entries: [
                SegmentDescriptor::from(0, 0, 0),
                SegmentDescriptor::from(0xa09b, 0, 0xfffff),
                SegmentDescriptor::from(0xc093, 0, 0xfffff),
                SegmentDescriptor::from(0x808b, 0, 0xfffff),
            ],
        }
    }

    pub fn code_segment(&self) -> kvm_bindings::kvm_segment {
        self.entries[1].to_kvm_segment(1)
    }

    pub fn data_segment(&self) -> kvm_bindings::kvm_segment {
        self.entries[2].to_kvm_segment(2)
    }

    pub fn tss_segment(&self) -> kvm_bindings::kvm_segment {
        self.entries[3].to_kvm_segment(3)
    }

    pub fn write_to_mem(&self, mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
        let base = vm_memory::GuestAddress(BOOT_GDT_OFFSET);
        for (i, entry) in self.entries.iter().enumerate() {
            let addr = base
                .checked_add((i * std::mem::size_of::<SegmentDescriptor>()) as u64)
                .ok_or_else(|| crate::error::Error::MemoryAccess("GDT address overflow".into()))?;
            mem.write_obj(*entry, addr)
                .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;
        }
        Ok(())
    }

    pub fn limit(&self) -> u16 {
        (std::mem::size_of_val(&self.entries) - 1) as u16
    }
}

impl Default for BootGdt {
    fn default() -> Self {
        Self::new()
    }
}

/// Write a single-entry IDT (unused vector table) for boot.
pub fn write_idt(mem: &vm_memory::GuestMemoryMmap<()>) -> crate::error::Result<()> {
    mem.write_obj(0u64, vm_memory::GuestAddress(BOOT_IDT_OFFSET))
        .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))
}
