#[derive(Debug, clap::Parser)]
#[command(name = "kitsune", about = "A KVM-based virtual machine monitor")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Start a virtual machine
    Run {
        /// Linux kernel image (ELF vmlinux or bzImage)
        #[arg(long, conflicts_with = "flat_binary")]
        kernel: Option<std::path::PathBuf>,

        /// Initial ramdisk image
        #[arg(long, requires = "kernel")]
        initrd: Option<std::path::PathBuf>,

        /// Raw disk image exposed as virtio-blk (/dev/vda)
        #[arg(long, requires = "kernel")]
        block: Option<std::path::PathBuf>,

        /// Host TAP interface name for virtio-net (e.g. tap0)
        #[arg(long, requires = "kernel")]
        tap: Option<String>,

        /// virtio-vsock guest CID and host Unix socket (`CID:PATH`, CID >= 3)
        #[arg(long, requires = "kernel", value_name = "CID:PATH")]
        vsock: Option<String>,

        /// Kernel command line
        #[arg(long, default_value = kitsune::DEFAULT_KERNEL_CMDLINE)]
        cmdline: String,

        /// Flat binary image loaded into guest physical memory (real mode)
        #[arg(long, conflicts_with = "kernel")]
        flat_binary: Option<std::path::PathBuf>,

        /// Guest memory size in MiB
        #[arg(
            long,
            default_value_t = 256,
            value_parser = clap::value_parser!(u32).range(1..=kitsune::MAX_MEMORY_MIB as i64)
        )]
        memory: u32,

        /// Number of guest vCPUs
        #[arg(
            long,
            default_value_t = 1,
            value_parser = clap::value_parser!(u8).range(1..=kitsune::MAX_VCPUS as i64)
        )]
        cpus: u8,

        /// Guest-physical load address (flat binary only)
        #[arg(long, default_value_t = 0)]
        load_addr: u64,

        /// Guest entry point with CS.base = 0 (flat binary only)
        #[arg(long, default_value_t = 0)]
        entry: u64,
    },
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    if let Err(err) = run(cli) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> kitsune::Result<()> {
    match cli.command {
        Command::Run {
            kernel,
            initrd,
            block,
            tap,
            vsock,
            cmdline,
            flat_binary,
            memory,
            cpus,
            load_addr,
            entry,
        } => {
            match (&kernel, &flat_binary) {
                (None, None) => {
                    eprintln!("error: either --kernel or --flat-binary is required");
                    std::process::exit(2);
                }
                (Some(_), None) if memory < 32 => {
                    eprintln!("error: kernel boot requires at least 32 MiB of memory");
                    std::process::exit(2);
                }
                (None, Some(_)) if cpus != 1 => {
                    eprintln!("error: --flat-binary supports only --cpus 1");
                    std::process::exit(2);
                }
                _ => {}
            }

            let config = kitsune::VmmConfig {
                mem_size: (memory as usize) * 1024 * 1024,
                num_vcpus: cpus,
            };
            let mut vmm = kitsune::Vmm::new(&config)?;

            if let Some(block) = block.as_ref() {
                vmm.add_block_device(block)?;
            }
            if let Some(tap) = tap.as_ref() {
                vmm.add_net_device(tap)?;
            }
            if let Some(vsock) = vsock.as_deref() {
                let Some((cid, path)) = split_vsock(vsock) else {
                    eprintln!("error: --vsock must be CID:PATH with CID >= 3");
                    std::process::exit(2);
                };
                vmm.add_vsock_device(cid, std::path::Path::new(path))?;
            }

            if let Some(kernel) = kernel {
                let cmdline = kitsune::build_kernel_cmdline(
                    &cmdline,
                    kitsune::KernelCmdlineOpts {
                        initrd: initrd.is_some(),
                        block: block.is_some(),
                        tap: tap.is_some(),
                        vsock: vsock.is_some(),
                    },
                );
                let boot = kitsune::KernelBootConfig {
                    kernel: &kernel,
                    initrd: initrd.as_deref(),
                    cmdline: &cmdline,
                };
                vmm.load_kernel(&boot)?;
            } else if let Some(flat_binary) = flat_binary {
                let image = std::fs::read(&flat_binary).map_err(kitsune::Error::ImageIo)?;
                vmm.load_flat_binary(&image, load_addr, entry)?;
            }

            vmm.run()?;
        }
    }
    Ok(())
}

/// Split `CID:PATH`. The path may itself contain colons.
fn split_vsock(spec: &str) -> Option<(u64, &str)> {
    let (cid, path) = spec.split_once(':')?;
    let cid = cid.parse().ok()?;
    if path.is_empty() {
        None
    } else {
        Some((cid, path))
    }
}
