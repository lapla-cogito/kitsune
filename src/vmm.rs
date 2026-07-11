use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

/// Virtual machine monitor instance.
///
/// Field order is intentional: `guest_mem` must outlive `vm`/`vcpu` so KVM
/// memslots are not left pointing at unmapped host pages on drop.
pub struct Vmm {
    _kvm: kvm_ioctls::Kvm,
    guest_mem: vm_memory::GuestMemoryMmap<()>,
    vm: kvm_ioctls::VmFd,
    vcpu: kvm_ioctls::VcpuFd,
    serial: crate::devices::SerialConsole,
    block: Option<crate::devices::VirtioBlock>,
    /// When true, `VcpuExit::Hlt` ends `run()`.
    /// Linux boots keep running so idle guests can still receive serial input.
    exit_on_hlt: bool,
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

        let serial = crate::devices::SerialConsole::new(&vm)?;

        Ok(Self {
            _kvm: kvm,
            guest_mem,
            vm,
            vcpu,
            serial,
            block: None,
            exit_on_hlt: true,
        })
    }

    /// Attach a virtio-blk device backed by the given host path.
    pub fn add_block_device(&mut self, path: &std::path::Path) -> crate::error::Result<()> {
        if self.block.is_some() {
            return Err(crate::error::Error::Block(
                "only one block device is supported".into(),
            ));
        }
        self.block = Some(crate::devices::VirtioBlock::new(path, &self.vm)?);
        Ok(())
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
        self.exit_on_hlt = true;
        Ok(())
    }

    /// Load a Linux kernel (ELF or bzImage), optional initrd, and cmdline.
    pub fn load_kernel(
        &mut self,
        config: &crate::boot::KernelBootConfig<'_>,
    ) -> crate::error::Result<()> {
        // Always install ACPI (MADT/IOAPIC + COM1). Virtio-mmio is optional.
        let virtio = self.block.as_ref().map(|_| crate::acpi::VirtioMmioAcpi {
            base: crate::devices::VirtioBlock::MMIO_BASE as u32,
            size: 0x1000,
            irq: crate::devices::VirtioBlock::IRQ,
            uid: 0,
        });
        crate::acpi::install_tables(&self.guest_mem, virtio.as_slice())?;

        let entry = crate::boot::load_linux(&self.guest_mem, config)?;
        crate::vcpu::setup_long_mode(&self.vcpu, &self.guest_mem, entry)?;
        self.exit_on_hlt = false;
        Ok(())
    }

    /// Run the guest until it shuts down.
    pub fn run(&mut self) -> crate::error::Result<()> {
        // Periodic SIGALRM interrupts KVM_RUN (EINTR) so we can poll host stdin
        // while the guest is blocked waiting for serial input. Without this,
        // in-kernel halt never returns to userspace and typed input is stuck.
        let _kick = KvmRunKickTimer::arm(20)?;

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
                kvm_ioctls::VcpuExit::MmioRead(addr, data) => {
                    if let Some(block) = self.block.as_ref()
                        && block.handles(addr)
                    {
                        block.read(addr, data);
                    }
                }
                kvm_ioctls::VcpuExit::MmioWrite(addr, data) => {
                    if let Some(block) = self.block.as_mut()
                        && block.handles(addr)
                    {
                        let mem = &self.guest_mem;
                        block.write(addr, data, mem)?;
                    }
                }
                kvm_ioctls::VcpuExit::Hlt => {
                    if self.exit_on_hlt {
                        break;
                    }
                    // Linux idle: avoid a tight spin when HLT exits to userspace.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                kvm_ioctls::VcpuExit::Shutdown => break,
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

/// Arms ITIMER_REAL so blocking `KVM_RUN` returns `EINTR` periodically.
struct KvmRunKickTimer;

impl KvmRunKickTimer {
    fn arm(period_ms: u64) -> crate::error::Result<Self> {
        // SAFETY: no-op handler without SA_RESTART so KVM_RUN returns EINTR.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = noop_sigalrm as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            if libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut()) != 0 {
                return Err(crate::error::Error::Serial(format!(
                    "sigaction SIGALRM: {}",
                    std::io::Error::last_os_error()
                )));
            }

            debug_assert!(period_ms < 1000);
            let usec = (period_ms * 1000) as libc::suseconds_t;
            let it = libc::itimerval {
                it_interval: libc::timeval {
                    tv_sec: 0,
                    tv_usec: usec,
                },
                it_value: libc::timeval {
                    tv_sec: 0,
                    tv_usec: usec,
                },
            };
            if libc::setitimer(libc::ITIMER_REAL, &it, std::ptr::null_mut()) != 0 {
                return Err(crate::error::Error::Serial(format!(
                    "setitimer: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        Ok(Self)
    }
}

impl Drop for KvmRunKickTimer {
    fn drop(&mut self) {
        // SAFETY: disarm the process-wide interval timer we armed.
        unsafe {
            let it: libc::itimerval = std::mem::zeroed();
            let _ = libc::setitimer(libc::ITIMER_REAL, &it, std::ptr::null_mut());
        }
    }
}

extern "C" fn noop_sigalrm(_: libc::c_int) {}
