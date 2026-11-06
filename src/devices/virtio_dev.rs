//! Shared virtio-mmio device shell.

/// Stack buffer passed to `VirtioState::copy_config`.
const CONFIG_CAP: usize = 64;

pub(crate) trait VirtioState: Send {
    fn mmio(&self) -> &crate::devices::virtio_mmio::VirtioMmio;
    fn mmio_mut(&mut self) -> &mut crate::devices::virtio_mmio::VirtioMmio;
    fn copy_config(&self, dest: &mut [u8]) -> usize;
}

enum WorkerSlot<T> {
    Idle,
    Starting,
    Running(std::thread::JoinHandle<T>),
}

pub(crate) struct WorkerReservation<T> {
    slot: std::sync::Arc<std::sync::Mutex<WorkerSlot<T>>>,
    active: bool,
}

impl<T> WorkerReservation<T> {
    pub(crate) fn commit(mut self, handle: std::thread::JoinHandle<T>) {
        let mut guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *guard = WorkerSlot::Running(handle);
        self.active = false;
    }
}

impl<T> Drop for WorkerReservation<T> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*guard, WorkerSlot::Starting) {
            *guard = WorkerSlot::Idle;
        }
    }
}

pub(crate) struct VirtioDev<S, T: Send + 'static> {
    base: u64,
    state: std::sync::Arc<std::sync::Mutex<S>>,
    kick: vmm_sys_util::eventfd::EventFd,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: std::sync::Arc<std::sync::Mutex<WorkerSlot<T>>>,
}

impl<S: VirtioState, T: Send + 'static> VirtioDev<S, T> {
    pub(crate) fn new(base: u64, state: S) -> crate::error::Result<Self> {
        let kick = state
            .mmio()
            .notify_fd()
            .try_clone()
            .map_err(|e| crate::error::Error::Virtio(e.to_string()))?;
        Ok(Self {
            base,
            state: std::sync::Arc::new(std::sync::Mutex::new(state)),
            kick,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker: std::sync::Arc::new(std::sync::Mutex::new(WorkerSlot::Idle)),
        })
    }

    pub(crate) fn range(&self) -> std::ops::Range<u64> {
        self.base..self.base + super::virtio_mmio::MMIO_SIZE
    }

    pub(crate) fn handles(&self, addr: u64) -> bool {
        self.range().contains(&addr)
    }

    pub(crate) fn state(&self) -> std::sync::Arc<std::sync::Mutex<S>> {
        std::sync::Arc::clone(&self.state)
    }

    pub(crate) fn read(&self, addr: u64, data: &mut [u8]) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut raw = [0u8; CONFIG_CAP];
        let n = state.copy_config(&mut raw).min(raw.len());
        state.mmio().read(addr, data, &raw[..n]);
    }

    pub(crate) fn write(
        &self,
        addr: u64,
        data: &[u8],
        mem: &vm_memory::GuestMemoryMmap<()>,
    ) -> crate::error::Result<()> {
        let notify = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.mmio_mut().write(addr, data, mem)?
        };
        if notify.is_some() {
            self.kick
                .write(1)
                .map_err(|e| crate::error::Error::Virtio(e.to_string()))?;
        }
        Ok(())
    }
}

impl<S, T: Send + 'static> VirtioDev<S, T> {
    pub(crate) fn clone_kick(&self) -> std::io::Result<vmm_sys_util::eventfd::EventFd> {
        self.kick.try_clone()
    }

    pub(crate) fn stop_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.stop)
    }

    pub(crate) fn clear_stop(&self) {
        self.stop.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn reserve(&self) -> Option<WorkerReservation<T>> {
        let slot = std::sync::Arc::clone(&self.worker);
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*guard, WorkerSlot::Idle) {
            return None;
        }
        *guard = WorkerSlot::Starting;
        drop(guard);
        Some(WorkerReservation { slot, active: true })
    }

    pub(crate) fn request_stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.kick.write(1);
    }

    pub(crate) fn take_worker(&self) -> Option<std::thread::JoinHandle<T>> {
        loop {
            let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
            match std::mem::replace(&mut *guard, WorkerSlot::Idle) {
                WorkerSlot::Running(handle) => return Some(handle),
                WorkerSlot::Idle => return None,
                WorkerSlot::Starting => {
                    *guard = WorkerSlot::Starting;
                    drop(guard);
                    std::thread::yield_now();
                }
            }
        }
    }
}

impl<S, T: Send + 'static> Drop for VirtioDev<S, T> {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.kick.write(1);
        if let Some(handle) = self.take_worker() {
            let _ = handle.join();
        }
    }
}
