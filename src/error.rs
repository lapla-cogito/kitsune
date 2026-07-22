#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to open /dev/kvm: {0}")]
    KvmOpen(#[source] kvm_ioctls::Error),

    #[error("KVM API version {found} is not supported (expected {expected})")]
    KvmApiVersion { found: i32, expected: i32 },

    #[error("KVM ioctl failed: {0}")]
    KvmIoctl(#[source] kvm_ioctls::Error),

    #[error("invalid guest memory size {0} (must be a non-zero multiple of 4096)")]
    InvalidMemorySize(usize),

    #[error(
        "guest memory size {size} bytes exceeds maximum {max} bytes \
         (must not overlap MMIO starting at {mmio_base:#x})"
    )]
    MemoryOverlapsMmio {
        size: usize,
        max: usize,
        mmio_base: u64,
    },

    #[error("invalid vCPU count {0} (must be 1..={1})")]
    InvalidVcpuCount(u8, u8),

    #[error("flat binary boot supports only one vCPU")]
    FlatBinaryMultiVcpu,

    #[error("vCPU thread failed: {0}")]
    VcpuThread(String),

    #[error("failed to allocate guest memory: {0}")]
    GuestMemory(String),

    #[error("failed to access guest memory: {0}")]
    MemoryAccess(String),

    #[error("failed to read guest image: {0}")]
    ImageIo(#[source] std::io::Error),

    #[error("guest image of {len} bytes does not fit at load address {load_addr:#x}")]
    ImageDoesNotFit { load_addr: u64, len: usize },

    #[error("failed to load kernel: {0}")]
    KernelLoad(String),

    #[error("failed to configure boot parameters: {0}")]
    BootConfigure(String),

    #[error("invalid kernel command line: {0}")]
    Cmdline(String),

    #[error("failed to set model-specific registers")]
    MsrSetup,

    #[error("serial device error: {0}")]
    Serial(String),

    #[error("block device error: {0}")]
    Block(String),

    #[error("network device error: {0}")]
    Net(String),

    #[error("unexpected vCPU exit: {0}")]
    UnexpectedExit(String),
}

pub type Result<T> = std::result::Result<T, Error>;
