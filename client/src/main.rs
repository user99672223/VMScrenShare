//! vmdesk client binary.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "client", version, about = "vmdesk remote desktop client")]
struct Cli {
    /// Signalling URL of the server (normally reached through an SSH tunnel).
    #[arg(long, default_value = proto::signalling::DEFAULT_SERVER_URL)]
    server: String,

    /// Video bitrate to request from the server, in kbit/s.
    #[arg(long)]
    bitrate: Option<u32>,

    /// Disable hardware decoding (D3D11VA / VA-API) and use FFmpeg's software h264 decoder.
    #[arg(long)]
    no_hwdec: bool,

    /// Increase log verbosity (-v: debug, -vv: trace). RUST_LOG overrides this.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let default = match cli.verbose {
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
    anyhow::bail!(
        "client not implemented yet (server={}, bitrate={:?}, no_hwdec={})",
        cli.server,
        cli.bitrate,
        cli.no_hwdec
    )
}
