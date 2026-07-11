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
        /// Flat binary image loaded into guest physical memory
        #[arg(long)]
        flat_binary: std::path::PathBuf,

        /// Guest memory size in MiB
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(1..))]
        memory: u32,

        /// Guest-physical address where the image is loaded
        #[arg(long, default_value_t = 0)]
        load_addr: u64,

        /// Guest entry point (physical address, real mode with CS.base = 0)
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
            flat_binary,
            memory,
            load_addr,
            entry,
        } => {
            let image = std::fs::read(&flat_binary).map_err(kitsune::Error::ImageIo)?;
            let config = kitsune::VmmConfig {
                mem_size: (memory as usize) * 1024 * 1024,
            };
            let mut vmm = kitsune::Vmm::new(&config)?;
            vmm.load_flat_binary(&image, load_addr, entry)?;
            vmm.run()?;
        }
    }
    Ok(())
}
