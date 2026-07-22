use vm_memory::GuestMemoryBackend as _;
use vm_memory::bytes::Bytes as _;

/// I/O port used as a guest -> host stop request in flat-binary mode (`exit_on_hlt`).
/// Compatible with QEMU `isa-debug-exit` (write 0 = success).
const DEBUG_EXIT_IOPORT: u16 = 0x501;

/// Signal used to force `KVM_RUN` to return `EINTR` on vCPU threads (stop / join).
const VCPU_KICK_SIGNUM: libc::c_int = libc::SIGUSR2;

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
    legacy_io: crate::devices::LegacyIo,
    block: Option<crate::devices::VirtioBlock>,
    net: Option<crate::devices::VirtioNet>,
    num_vcpus: u8,
    /// When true, `VcpuExit::Hlt` ends `run()`.
    exit_on_hlt: bool,
    /// When true, HLT/PAUSE/MWAIT are handled in-kernel (no userspace exit).
    idle_exits_disabled: bool,
}

impl Vmm {
    /// Create a VM with guest memory, IRQ chip, PIT, and `config.num_vcpus` vCPUs.
    pub fn new(config: &crate::config::VmmConfig) -> crate::error::Result<Self> {
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
            legacy_io: crate::devices::LegacyIo::new(),
            block: None,
            net: None,
            num_vcpus: config.num_vcpus,
            exit_on_hlt: true,
            idle_exits_disabled: false,
        })
    }

    /// Prefer in-kernel idle instructions so guests do not exit to userspace on HLT/PAUSE/MWAIT.
    /// Tries the richest flag set first, then falls back.
    ///
    /// Flat-binary mode must keep HLT exits (`exit_on_hlt`); only call this for direct kernel boot.
    fn try_disable_idle_exits(vm: &kvm_ioctls::VmFd) -> bool {
        let cap_id = kvm_bindings::KVM_CAP_X86_DISABLE_EXITS as i32;
        if vm.check_extension_raw(cap_id as libc::c_ulong) <= 0 {
            return false;
        }

        let attempts = [
            kvm_bindings::KVM_X86_DISABLE_EXITS_HLT
                | kvm_bindings::KVM_X86_DISABLE_EXITS_PAUSE
                | kvm_bindings::KVM_X86_DISABLE_EXITS_MWAIT,
            kvm_bindings::KVM_X86_DISABLE_EXITS_HLT | kvm_bindings::KVM_X86_DISABLE_EXITS_PAUSE,
            kvm_bindings::KVM_X86_DISABLE_EXITS_HLT,
        ];

        for flags in attempts {
            let mut cap = kvm_bindings::kvm_enable_cap {
                cap: kvm_bindings::KVM_CAP_X86_DISABLE_EXITS,
                ..Default::default()
            };
            cap.args[0] = u64::from(flags);
            if vm.enable_cap(&cap).is_ok() {
                return true;
            }
        }
        false
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
        self.idle_exits_disabled = false;
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

        self.idle_exits_disabled = Self::try_disable_idle_exits(&self.vm);
        self.exit_on_hlt = false;
        Ok(())
    }

    /// Run all vCPUs until the guest shuts down.
    pub fn run(&mut self) -> crate::error::Result<()> {
        let _kick_handler = VcpuKickHandlerGuard::install()?;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let kick = std::sync::Arc::new(VcpuKickRegistry::new());
        let serial = std::sync::Arc::new(std::sync::Mutex::new(
            self.serial
                .take()
                .expect("serial console is installed at construction"),
        ));
        let stdin_worker = crate::devices::StdinWorker::start(std::sync::Arc::clone(&serial))?;

        let legacy_io =
            std::sync::Arc::new(std::sync::Mutex::new(std::mem::take(&mut self.legacy_io)));

        let block = self.block.take().map(std::sync::Arc::new);
        if let Some(block) = block.as_ref() {
            block.start_worker(self.guest_mem.clone())?;
        }
        let mem = self.guest_mem.clone();
        let net = self.net.take().map(std::sync::Arc::new);
        if let Some(net) = net.as_ref() {
            net.start_worker(mem.clone())?;
        }
        let exit_on_hlt = self.exit_on_hlt;
        let idle_exits_disabled = self.idle_exits_disabled;

        let mut vcpus = std::mem::take(&mut self.vcpus);
        let bsp = vcpus.remove(0);

        let mut handles = Vec::with_capacity(vcpus.len());
        for (i, vcpu) in vcpus.into_iter().enumerate() {
            let id = (i + 1) as u8;
            let ctx = VcpuRunCtx {
                id,
                exit_on_hlt: false,
                idle_exits_disabled,
                stop: std::sync::Arc::clone(&stop),
                kick: std::sync::Arc::clone(&kick),
                serial: std::sync::Arc::clone(&serial),
                legacy_io: std::sync::Arc::clone(&legacy_io),
                block: block.clone(),
                net: net.clone(),
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
                exit_on_hlt,
                idle_exits_disabled,
                stop: std::sync::Arc::clone(&stop),
                kick: std::sync::Arc::clone(&kick),
                serial: std::sync::Arc::clone(&serial),
                legacy_io: std::sync::Arc::clone(&legacy_io),
                block: block.clone(),
                net: net.clone(),
                mem,
            },
        );
        // Ensure AP threads leave their KVM_RUN loops.
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        kick.kick_all();

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

        if let Err(e) = stdin_worker.stop()
            && first_err.is_none()
        {
            first_err = Some(e);
        }
        if let Some(block) = block.as_ref()
            && let Err(e) = block.stop_worker()
            && first_err.is_none()
        {
            first_err = Some(e);
        }
        if let Some(net) = net.as_ref()
            && let Err(e) = net.stop_worker()
            && first_err.is_none()
        {
            first_err = Some(e);
        }

        if let Ok(s) = std::sync::Arc::try_unwrap(serial) {
            self.serial = Some(s.into_inner().unwrap_or_else(|e| e.into_inner()));
        }
        if let Ok(io) = std::sync::Arc::try_unwrap(legacy_io) {
            self.legacy_io = io.into_inner().unwrap_or_else(|e| e.into_inner());
        }
        self.block = block.and_then(|a| std::sync::Arc::try_unwrap(a).ok());
        self.net = net.and_then(|a| std::sync::Arc::try_unwrap(a).ok());

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

struct VcpuRunCtx {
    id: u8,
    exit_on_hlt: bool,
    idle_exits_disabled: bool,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    kick: std::sync::Arc<VcpuKickRegistry>,
    serial: std::sync::Arc<std::sync::Mutex<crate::devices::SerialConsole>>,
    legacy_io: std::sync::Arc<std::sync::Mutex<crate::devices::LegacyIo>>,
    block: Option<std::sync::Arc<crate::devices::VirtioBlock>>,
    net: Option<std::sync::Arc<crate::devices::VirtioNet>>,
    mem: vm_memory::GuestMemoryMmap<()>,
}

fn run_vcpu_loop(mut vcpu: kvm_ioctls::VcpuFd, ctx: VcpuRunCtx) -> crate::error::Result<()> {
    let _registration = ctx.kick.register();

    while !ctx.stop.load(std::sync::atomic::Ordering::Relaxed) {
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            // EINTR: on-demand kick (stop). EAGAIN: AP not yet runnable (Wait-For-SIPI).
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                if e.errno() == libc::EAGAIN {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                continue;
            }
            Err(e) => {
                request_stop(&ctx);
                return Err(crate::error::Error::KvmIoctl(e));
            }
        };

        match exit {
            kvm_ioctls::VcpuExit::IoOut(port, data) => {
                if ctx.exit_on_hlt && port == DEBUG_EXIT_IOPORT {
                    request_stop(&ctx);
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
                } else if crate::devices::LegacyIo::handles_port(port) {
                    let action = ctx
                        .legacy_io
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .bus_write(port, data);
                    if action == crate::devices::PowerAction::Reset {
                        request_stop(&ctx);
                        break;
                    }
                }
            }
            kvm_ioctls::VcpuExit::IoIn(port, data) => {
                if crate::devices::SerialConsole::handles_port(port) {
                    ctx.serial
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .bus_read(port, data);
                } else if crate::devices::LegacyIo::handles_port(port) {
                    ctx.legacy_io
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .bus_read(port, data);
                } else {
                    data.fill(0xff);
                }
            }
            kvm_ioctls::VcpuExit::MmioRead(addr, data) => {
                if let Some(dev) = ctx.block.as_ref()
                    && dev.handles(addr)
                {
                    dev.read(addr, data);
                    continue;
                }
                if let Some(net) = ctx.net.as_ref()
                    && net.handles(addr)
                {
                    net.read(addr, data);
                }
            }
            kvm_ioctls::VcpuExit::MmioWrite(addr, data) => {
                if let Some(dev) = ctx.block.as_ref()
                    && dev.handles(addr)
                {
                    dev.write(addr, data, &ctx.mem)?;
                    continue;
                }
                if let Some(net) = ctx.net.as_ref()
                    && net.handles(addr)
                {
                    net.write(addr, data, &ctx.mem)?;
                }
            }
            kvm_ioctls::VcpuExit::Hlt => {
                if ctx.exit_on_hlt {
                    request_stop(&ctx);
                    break;
                }
                if ctx.idle_exits_disabled {
                    continue;
                }
                idle_wait(&ctx);
            }
            kvm_ioctls::VcpuExit::Shutdown => {
                request_stop(&ctx);
                break;
            }
            kvm_ioctls::VcpuExit::SystemEvent(event_type, _) => match event_type {
                kvm_bindings::KVM_SYSTEM_EVENT_SHUTDOWN | kvm_bindings::KVM_SYSTEM_EVENT_RESET => {
                    request_stop(&ctx);
                    break;
                }
                other => {
                    request_stop(&ctx);
                    return Err(crate::error::Error::UnexpectedExit(format!(
                        "vcpu{} system event {other}",
                        ctx.id
                    )));
                }
            },
            other => {
                request_stop(&ctx);
                return Err(crate::error::Error::UnexpectedExit(format!(
                    "vcpu{} {other:?}",
                    ctx.id
                )));
            }
        }
    }
    Ok(())
}

fn request_stop(ctx: &VcpuRunCtx) {
    ctx.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    ctx.kick.kick_all();
}

/// Interruptible idle when userspace still sees HLT exits.
fn idle_wait(ctx: &VcpuRunCtx) {
    const SLICE: std::time::Duration = std::time::Duration::from_millis(1);
    const SLICES: u32 = 10; // ~10 ms total before re-entering KVM_RUN
    for _ in 0..SLICES {
        if ctx.stop.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        std::thread::sleep(SLICE);
    }
}

/// Tracks live vCPU threads so stop can interrupt blocking `KVM_RUN`.
struct VcpuKickRegistry {
    threads: std::sync::Mutex<Vec<libc::pthread_t>>,
}

impl VcpuKickRegistry {
    fn new() -> Self {
        Self {
            threads: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn register(self: &std::sync::Arc<Self>) -> VcpuKickRegistration {
        // SAFETY: pthread_self is always valid for the calling thread.
        let tid = unsafe { libc::pthread_self() };
        self.threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tid);
        VcpuKickRegistration {
            registry: std::sync::Arc::clone(self),
            tid,
        }
    }

    fn kick_all(&self) {
        let threads = self.threads.lock().unwrap_or_else(|e| e.into_inner());
        for &tid in threads.iter() {
            // SAFETY: tid was recorded from a live vCPU thread; SIGUSR2 has a no-op handler.
            unsafe {
                let _ = libc::pthread_kill(tid, VCPU_KICK_SIGNUM);
            }
        }
    }
}

struct VcpuKickRegistration {
    registry: std::sync::Arc<VcpuKickRegistry>,
    tid: libc::pthread_t,
}

impl Drop for VcpuKickRegistration {
    fn drop(&mut self) {
        let mut threads = self
            .registry
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        threads.retain(|&t| t != self.tid);
    }
}

/// Installs a temporary SIGUSR2 handler for the duration of [`Vmm::run`].
struct VcpuKickHandlerGuard {
    prev: libc::sigaction,
}

impl VcpuKickHandlerGuard {
    fn install() -> crate::error::Result<Self> {
        // SAFETY: no-op handler without SA_RESTART so KVM_RUN returns EINTR on kick.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = noop_vcpu_kick as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            let mut prev: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(VCPU_KICK_SIGNUM, &sa, &mut prev) != 0 {
                return Err(crate::error::Error::Serial(format!(
                    "sigaction SIGUSR2: {}",
                    std::io::Error::last_os_error()
                )));
            }
            Ok(Self { prev })
        }
    }
}

impl Drop for VcpuKickHandlerGuard {
    fn drop(&mut self) {
        // SAFETY: restore the previous disposition installed before run().
        unsafe {
            let _ = libc::sigaction(VCPU_KICK_SIGNUM, &self.prev, std::ptr::null_mut());
        }
    }
}

extern "C" fn noop_vcpu_kick(_: libc::c_int) {}
