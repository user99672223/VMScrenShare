//! vmdesk server binary. See the README for the full setup flow.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "server",
    version,
    about = "vmdesk remote desktop server (runs on the VM)"
)]
struct Cli {
    /// Increase log verbosity (-v: debug, -vv: trace). RUST_LOG overrides this.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the server (default): capture, encode, serve WebRTC and inject input.
    Run,
    /// One-time system setup (root): Xorg on vkms, XFCE, lightdm autologin, udev, iptables, systemd.
    Setup,
    /// Check the machine and print PASS/FAIL per item with the fix for each failure.
    Doctor,
    /// Capture one frame from the vkms framebuffer and write it as PNG.
    Capture {
        /// Output path of the PNG file.
        #[arg(long)]
        png: std::path::PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => anyhow::bail!("run: not implemented yet"),
        Command::Setup => anyhow::bail!("setup: not implemented yet"),
        Command::Doctor => anyhow::bail!("doctor: not implemented yet"),
        Command::Capture { png } => {
            anyhow::bail!("capture --png {}: not implemented yet", png.display())
        }
    }
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    let _ = proto::PROTOCOL_VERSION;
}
