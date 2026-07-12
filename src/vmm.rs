use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

/// I/O port used as a guest -> host stop request in flat-binary mode (`exit_on_hlt`).
/// Compatible with QEMU `isa-debug-exit` (write 0 = success).
const DEBUG_EXIT_IOPORT: u16 = 0x501;

/// Virtual machine monitor instance.
///
/// Field order is intentional: `guest_mem` must outlive `vm`/`vcpus` so KVM
/// memslots are not left pointing at unmapped host pages on drop.
pub struct Vmm {
    _kvm: kvm_ioctls::Kvm,
    guest_mem: vm_memory::GuestMemoryMmap<()>,
    vm: kvm_ioctls::VmFd,
    vcpus: Vec<kvm_ioctls::VcpuFd>,
    serial: Option<crate::devices::SerialConsole>,
    block: Option<crate::devices::VirtioBlock>,
    net: Option<crate::devices::VirtioNet>,
    num_vcpus: u8,
    /// When true, `VcpuExit::Hlt` ends `run()`.
    exit_on_hlt: bool,
}

impl Vmm {
    /// Create a VM with guest memory, IRQ chip, PIT, and `config.num_vcpus` vCPUs.
    pub fn new(config: &crate::config::VmmConfig) -> crate::error::Result<Self> {
        if config.mem_size == 0 || !config.mem_size.is_multiple_of(4096) {
            return Err(crate::error::Error::InvalidMemorySize(config.mem_size));
        }
        if config.num_vcpus == 0 || config.num_vcpus > crate::config::MAX_VCPUS {
            return Err(crate::error::Error::InvalidVcpuCount(
                config.num_vcpus,
                crate::config::MAX_VCPUS,
            ));
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

        let mut vcpus = Vec::with_capacity(usize::from(config.num_vcpus));
        for id in 0..config.num_vcpus {
            let vcpu = vm
                .create_vcpu(u64::from(id))
                .map_err(crate::error::Error::KvmIoctl)?;
            crate::vcpu::setup_cpuid(&kvm, &vcpu, id, config.num_vcpus)?;
            vcpus.push(vcpu);
        }

        let serial = crate::devices::SerialConsole::new(&vm)?;

        Ok(Self {
            _kvm: kvm,
            guest_mem,
            vm,
            vcpus,
            serial: Some(serial),
            block: None,
            net: None,
            num_vcpus: config.num_vcpus,
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

    /// Attach a virtio-net device backed by the given host TAP interface.
    pub fn add_net_device(&mut self, tap_ifname: &str) -> crate::error::Result<()> {
        if self.net.is_some() {
            return Err(crate::error::Error::Net(
                "only one network device is supported".into(),
            ));
        }
        self.net = Some(crate::devices::VirtioNet::new(tap_ifname, &self.vm)?);
        Ok(())
    }

    /// Load a flat binary into guest memory and set the real-mode entry point.
    pub fn load_flat_binary(
        &mut self,
        image: &[u8],
        load_addr: u64,
        entry: u64,
    ) -> crate::error::Result<()> {
        if self.num_vcpus != 1 {
            return Err(crate::error::Error::FlatBinaryMultiVcpu);
        }
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

        crate::vcpu::setup_real_mode(&self.vcpus[0], entry)?;
        self.exit_on_hlt = true;
        Ok(())
    }

    /// Load a Linux kernel (ELF or bzImage), optional initrd, and cmdline.
    pub fn load_kernel(
        &mut self,
        config: &crate::boot::KernelBootConfig<'_>,
    ) -> crate::error::Result<()> {
        let mut virtio = Vec::new();
        if self.block.is_some() {
            virtio.push(crate::acpi::VirtioMmioAcpi {
                base: crate::devices::VirtioBlock::MMIO_BASE as u32,
                size: 0x1000,
                irq: crate::devices::VirtioBlock::IRQ,
                uid: 0,
            });
        }
        if self.net.is_some() {
            virtio.push(crate::acpi::VirtioMmioAcpi {
                base: crate::devices::VirtioNet::MMIO_BASE as u32,
                size: 0x1000,
                irq: crate::devices::VirtioNet::IRQ,
                uid: 1,
            });
        }
        crate::acpi::install_tables(&self.guest_mem, &virtio, self.num_vcpus)?;

        let entry = crate::boot::load_linux(&self.guest_mem, config)?;

        // BSP runs the kernel entry in long mode.
        crate::vcpu::setup_long_mode(&self.vcpus[0], &self.guest_mem, entry)?;

        // APs wait for INIT/SIPI from the BSP (handled by the in-kernel irqchip).
        for ap in self.vcpus.iter().skip(1) {
            let mp = kvm_bindings::kvm_mp_state {
                mp_state: kvm_bindings::KVM_MP_STATE_UNINITIALIZED,
            };
            ap.set_mp_state(mp).map_err(crate::error::Error::KvmIoctl)?;
        }

        self.exit_on_hlt = false;
        Ok(())
    }

    /// Run all vCPUs until the guest shuts down.
    ///
    /// The BSP runs on this thread (so the SIGALRM kick reaches its `KVM_RUN`).
    /// Application processors run on background threads.
    pub fn run(&mut self) -> crate::error::Result<()> {
        let _kick = KvmRunKickTimer::arm(20)?;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let serial = std::sync::Arc::new(std::sync::Mutex::new(
            self.serial
                .take()
                .expect("serial console is installed at construction"),
        ));
        let block = std::sync::Arc::new(std::sync::Mutex::new(self.block.take()));
        let net = std::sync::Arc::new(std::sync::Mutex::new(self.net.take()));
        let mem = self.guest_mem.clone();
        let exit_on_hlt = self.exit_on_hlt;

        let mut vcpus = std::mem::take(&mut self.vcpus);
        let bsp = vcpus.remove(0);

        let mut handles = Vec::with_capacity(vcpus.len());
        for (i, vcpu) in vcpus.into_iter().enumerate() {
            let id = (i + 1) as u8;
            let ctx = VcpuRunCtx {
                id,
                is_bsp: false,
                exit_on_hlt: false,
                stop: std::sync::Arc::clone(&stop),
                serial: std::sync::Arc::clone(&serial),
                block: std::sync::Arc::clone(&block),
                net: std::sync::Arc::clone(&net),
                mem: mem.clone(),
            };
            handles.push(
                std::thread::Builder::new()
                    .name(format!("vcpu-{id}"))
                    .spawn(move || run_vcpu_loop(vcpu, ctx))
                    .map_err(|e| crate::error::Error::VcpuThread(e.to_string()))?,
            );
        }

        let bsp_result = run_vcpu_loop(
            bsp,
            VcpuRunCtx {
                id: 0,
                is_bsp: true,
                exit_on_hlt,
                stop: std::sync::Arc::clone(&stop),
                serial: std::sync::Arc::clone(&serial),
                block: std::sync::Arc::clone(&block),
                net: std::sync::Arc::clone(&net),
                mem,
            },
        );
        // Ensure AP threads leave their KVM_RUN loops.
        stop.store(true, std::sync::atomic::Ordering::SeqCst);

        let mut first_err = bsp_result.err();
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(crate::error::Error::VcpuThread(
                            "vCPU thread panicked".into(),
                        ));
                    }
                }
            }
        }

        if let Ok(s) = std::sync::Arc::try_unwrap(serial) {
            self.serial = Some(s.into_inner().unwrap_or_else(|e| e.into_inner()));
        }
        if let Ok(b) = std::sync::Arc::try_unwrap(block) {
            self.block = b.into_inner().unwrap_or_else(|e| e.into_inner());
        }
        if let Ok(n) = std::sync::Arc::try_unwrap(net) {
            self.net = n.into_inner().unwrap_or_else(|e| e.into_inner());
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

struct VcpuRunCtx {
    id: u8,
    is_bsp: bool,
    exit_on_hlt: bool,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    serial: std::sync::Arc<std::sync::Mutex<crate::devices::SerialConsole>>,
    block: std::sync::Arc<std::sync::Mutex<Option<crate::devices::VirtioBlock>>>,
    net: std::sync::Arc<std::sync::Mutex<Option<crate::devices::VirtioNet>>>,
    mem: vm_memory::GuestMemoryMmap<()>,
}

fn run_vcpu_loop(mut vcpu: kvm_ioctls::VcpuFd, ctx: VcpuRunCtx) -> crate::error::Result<()> {
    while !ctx.stop.load(std::sync::atomic::Ordering::Relaxed) {
        if ctx.is_bsp {
            ctx.serial
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .poll_stdin()?;
            let mut net = ctx.net.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(net) = net.as_mut() {
                net.poll_tap(&ctx.mem)?;
            }
        }

        let exit = match vcpu.run() {
            Ok(exit) => exit,
            // EINTR: kick timer. EAGAIN: AP not yet runnable (Wait-For-SIPI).
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                if e.errno() == libc::EAGAIN {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                continue;
            }
            Err(e) => {
                ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                return Err(crate::error::Error::KvmIoctl(e));
            }
        };

        match exit {
            kvm_ioctls::VcpuExit::IoOut(port, data) => {
                if ctx.exit_on_hlt && port == DEBUG_EXIT_IOPORT {
                    ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    let code = data.first().copied().unwrap_or(0);
                    if code != 0 {
                        return Err(crate::error::Error::UnexpectedExit(format!(
                            "vcpu{} debug exit status {code}",
                            ctx.id
                        )));
                    }
                    break;
                }
                if crate::devices::SerialConsole::handles_port(port) {
                    ctx.serial
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .bus_write(port, data)?;
                }
            }
            kvm_ioctls::VcpuExit::IoIn(port, data) => {
                if crate::devices::SerialConsole::handles_port(port) {
                    ctx.serial
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .bus_read(port, data);
                } else {
                    data.fill(0xff);
                }
            }
            kvm_ioctls::VcpuExit::MmioRead(addr, data) => {
                {
                    let guard = ctx.block.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(dev) = guard.as_ref()
                        && dev.handles(addr)
                    {
                        dev.read(addr, data);
                        continue;
                    }
                }
                let guard = ctx.net.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(dev) = guard.as_ref()
                    && dev.handles(addr)
                {
                    dev.read(addr, data);
                }
            }
            kvm_ioctls::VcpuExit::MmioWrite(addr, data) => {
                {
                    let mut guard = ctx.block.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(dev) = guard.as_mut()
                        && dev.handles(addr)
                    {
                        dev.write(addr, data, &ctx.mem)?;
                        continue;
                    }
                }
                let mut guard = ctx.net.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(dev) = guard.as_mut()
                    && dev.handles(addr)
                {
                    dev.write(addr, data, &ctx.mem)?;
                }
            }
            kvm_ioctls::VcpuExit::Hlt => {
                if ctx.exit_on_hlt {
                    ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            kvm_ioctls::VcpuExit::Shutdown => {
                ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            kvm_ioctls::VcpuExit::SystemEvent(event_type, _) => match event_type {
                kvm_bindings::KVM_SYSTEM_EVENT_SHUTDOWN | kvm_bindings::KVM_SYSTEM_EVENT_RESET => {
                    ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                other => {
                    ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    return Err(crate::error::Error::UnexpectedExit(format!(
                        "vcpu{} system event {other}",
                        ctx.id
                    )));
                }
            },
            other => {
                ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                return Err(crate::error::Error::UnexpectedExit(format!(
                    "vcpu{} {other:?}",
                    ctx.id
                )));
            }
        }
    }
    Ok(())
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
