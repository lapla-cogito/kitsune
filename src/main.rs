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

        /// Kernel command line
        #[arg(long, default_value = kitsune::DEFAULT_KERNEL_CMDLINE)]
        cmdline: String,

        /// Flat binary image loaded into guest physical memory (real mode)
        #[arg(long, conflicts_with = "kernel")]
        flat_binary: Option<std::path::PathBuf>,

        /// Guest memory size in MiB
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(1..))]
        memory: u32,

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
            cmdline,
            flat_binary,
            memory,
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
                _ => {}
            }

            let config = kitsune::VmmConfig {
                mem_size: (memory as usize) * 1024 * 1024,
            };
            let mut vmm = kitsune::Vmm::new(&config)?;

            if let Some(kernel) = kernel {
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
