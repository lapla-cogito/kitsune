use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

/// Virtual machine monitor instance.
///
/// Field order is intentional: `guest_mem` must outlive `vm`/`vcpu` so KVM
/// memslots are not left pointing at unmapped host pages on drop.
pub struct Vmm {
    _kvm: kvm_ioctls::Kvm,
    guest_mem: vm_memory::GuestMemoryMmap<()>,
    _vm: kvm_ioctls::VmFd,
    vcpu: kvm_ioctls::VcpuFd,
    serial: crate::devices::SerialConsole,
}

impl Vmm {
    /// Create a VM with guest memory, IRQ chip, PIT, and a single vCPU.
    pub fn new(config: &crate::config::VmmConfig) -> crate::error::Result<Self> {
        if config.mem_size == 0 || !config.mem_size.is_multiple_of(4096) {
            return Err(crate::error::Error::InvalidMemorySize(config.mem_size));
        }

        let kvm = kvm_ioctls::Kvm::new().map_err(crate::error::Error::KvmOpen)?;
        let api_version = kvm.get_api_version();
        if api_version as u32 != kvm_bindings::KVM_API_VERSION {
            return Err(crate::error::Error::KvmApiVersion {
                found: api_version,
                expected: kvm_bindings::KVM_API_VERSION as i32,
            });
        }

        let vm = kvm.create_vm().map_err(crate::error::Error::KvmIoctl)?;
        vm.set_tss_address(crate::boot::KVM_TSS_ADDRESS)
            .map_err(crate::error::Error::KvmIoctl)?;
        vm.create_irq_chip()
            .map_err(crate::error::Error::KvmIoctl)?;
        let pit_config = kvm_bindings::kvm_pit_config {
            flags: kvm_bindings::KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        vm.create_pit2(pit_config)
            .map_err(crate::error::Error::KvmIoctl)?;

        let guest_mem = crate::memory::create_guest_memory(&vm, config.mem_size)?;
        let vcpu = vm.create_vcpu(0).map_err(crate::error::Error::KvmIoctl)?;

        let cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .map_err(crate::error::Error::KvmIoctl)?;
        vcpu.set_cpuid2(&cpuid)
            .map_err(crate::error::Error::KvmIoctl)?;

        Ok(Self {
            _kvm: kvm,
            guest_mem,
            _vm: vm,
            vcpu,
            serial: crate::devices::SerialConsole::new(),
        })
    }

    /// Load a flat binary into guest memory and set the real-mode entry point.
    pub fn load_flat_binary(
        &mut self,
        image: &[u8],
        load_addr: u64,
        entry: u64,
    ) -> crate::error::Result<()> {
        if !self
            .guest_mem
            .check_range(vm_memory::GuestAddress(load_addr), image.len())
        {
            return Err(crate::error::Error::ImageDoesNotFit {
                load_addr,
                len: image.len(),
            });
        }

        self.guest_mem
            .write_slice(image, vm_memory::GuestAddress(load_addr))
            .map_err(|e| crate::error::Error::MemoryAccess(e.to_string()))?;

        crate::vcpu::setup_real_mode(&self.vcpu, entry)?;
        Ok(())
    }

    /// Load a Linux kernel (ELF or bzImage), optional initrd, and cmdline.
    pub fn load_kernel(
        &mut self,
        config: &crate::boot::KernelBootConfig<'_>,
    ) -> crate::error::Result<()> {
        let entry = crate::boot::load_linux(&self.guest_mem, config)?;
        crate::vcpu::setup_long_mode(&self.vcpu, &self.guest_mem, entry)?;
        Ok(())
    }

    /// Run the guest until it halts or shuts down.
    pub fn run(&mut self) -> crate::error::Result<()> {
        loop {
            self.serial.poll_stdin()?;

            let exit = match self.vcpu.run() {
                Ok(exit) => exit,
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => return Err(crate::error::Error::KvmIoctl(e)),
            };

            match exit {
                kvm_ioctls::VcpuExit::IoOut(port, data) => {
                    if crate::devices::SerialConsole::handles_port(port) {
                        self.serial.bus_write(port, data)?;
                    }
                }
                kvm_ioctls::VcpuExit::IoIn(port, data) => {
                    if crate::devices::SerialConsole::handles_port(port) {
                        self.serial.bus_read(port, data);
                    } else {
                        data.fill(0xff);
                    }
                }
                kvm_ioctls::VcpuExit::Hlt | kvm_ioctls::VcpuExit::Shutdown => break,
                kvm_ioctls::VcpuExit::SystemEvent(event_type, _) => match event_type {
                    kvm_bindings::KVM_SYSTEM_EVENT_SHUTDOWN
                    | kvm_bindings::KVM_SYSTEM_EVENT_RESET => break,
                    other => {
                        return Err(crate::error::Error::UnexpectedExit(format!(
                            "system event {other}"
                        )));
                    }
                },
                other => {
                    return Err(crate::error::Error::UnexpectedExit(format!("{other:?}")));
                }
            }
        }
        Ok(())
    }
}
