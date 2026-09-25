//! vmdesk server binary. See the README for the full setup flow.

mod capture;
mod config;
mod convert;
mod doctor;
mod encoder;
mod input;
mod metadata;
mod netinfo;
mod pipeline;
mod png_out;
mod rtcp_forward;
mod session;
mod setup;
mod signalling;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;

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

    /// Configuration file.
    #[arg(long, global = true, default_value = config::DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the server (default): capture, encode, serve WebRTC and inject input.
    Run,
    /// One-time system setup (root): Xorg on vkms, XFCE, lightdm autologin, udev, iptables, systemd.
    /// Re-run with --skip-apt after updating the binary; existing config values are kept.
    Setup {
        /// Desktop user: lightdm autologin and the account the service runs as.
        #[arg(long, default_value = "ubuntu")]
        user: String,
        /// Do not run apt-get (packages already installed).
        #[arg(long)]
        skip_apt: bool,
        /// Where to install this binary for the systemd service.
        #[arg(long, default_value = setup::INSTALL_PATH)]
        install_path: PathBuf,
        /// Public IPv4 of the VM, written to network.public_ip (use when the OCI metadata has none).
        #[arg(long)]
        public_ip: Option<String>,
        /// IPv6 address to advertise instead of the interface's global address (network.public_ipv6).
        #[arg(long)]
        public_ipv6: Option<String>,
    },
    /// Check the machine and print PASS/FAIL per item with the fix for each failure.
    Doctor,
    /// Capture one frame from the vkms framebuffer and write it as PNG.
    Capture {
        /// Output path of the PNG file.
        #[arg(long)]
        png: PathBuf,
        /// How long to wait for the display to become active.
        #[arg(long, default_value = "10")]
        timeout_secs: u64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let config = Config::load(&cli.config)?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(config),
        Command::Setup {
            user,
            skip_apt,
            install_path,
            public_ip,
            public_ipv6,
        } => setup::run(
            &setup::SetupOptions {
                user,
                config_path: cli.config.clone(),
                skip_apt,
                install_path,
                public_ip,
                public_ipv6,
            },
            &config,
        ),
        Command::Doctor => {
            if doctor::run(&config)? {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Command::Capture { png, timeout_secs } => capture_png(&config, &png, timeout_secs),
    }
}

fn run(config: Config) -> Result<()> {
    let config = Arc::new(config);
    let shared = Arc::new(pipeline::Shared::new(config.video.bitrate_kbps));
    pipeline::spawn(Arc::clone(&config), Arc::clone(&shared)).context("starting pipeline")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("vmdesk-rt")
        .enable_all()
        .build()
        .context("creating tokio runtime")?;
    runtime.block_on(async move {
        let public_ipv4 = if config.network.public_ip.is_empty() {
            match metadata::detect_public_ip(&config.network.metadata_url).await {
                Ok(ip) => {
                    tracing::info!("public IPv4 {ip} (OCI metadata)");
                    Some(ip)
                }
                Err(e) => {
                    tracing::warn!(
                        "public IPv4 not detected ({e:#}); will retry on the first offer. \
                         If the metadata has no publicIp: sudo ./server setup --skip-apt --public-ip <ip>"
                    );
                    None
                }
            }
        } else {
            tracing::info!("public IPv4 {} (config)", config.network.public_ip);
            Some(config.network.public_ip.clone())
        };
        let local_ipv6 = netinfo::global_ipv6();
        let public_ipv6 = (!config.network.public_ipv6.is_empty())
            .then(|| config.network.public_ipv6.clone());
        let net = session::NetInfo {
            public_ipv4,
            public_ipv6,
            local_ipv6,
            ipv6_enabled: config.network.ipv6,
        };
        match (net.ipv6_enabled, local_ipv6, &net.public_ipv6) {
            (false, _, _) => tracing::info!("IPv6 disabled in config"),
            (true, _, Some(ip)) => tracing::info!("IPv6 {ip} (config override) will be advertised"),
            (true, Some(ip), None) => tracing::info!("IPv6 {ip} (interface) will be advertised"),
            (true, None, None) => tracing::info!("no global IPv6 address found: IPv4 only"),
        }
        let input: session::SharedInput = Arc::new(Mutex::new(input::create_sink()));
        let manager = Arc::new(session::SessionManager::new(
            Arc::clone(&config),
            shared,
            input,
            net,
        ));
        signalling::serve(config.network.signalling_addr, manager).await
    })
}

fn capture_png(config: &Config, png: &std::path::Path, timeout_secs: u64) -> Result<()> {
    let mut source = capture::wait_for_display(
        &config.capture.card,
        &config.capture.connector,
        Some(Duration::from_secs(timeout_secs)),
    )?;
    let frame = source.capture().context("capturing frame")?;
    tracing::info!(
        "captured {}x{} {:?} pitch {}",
        frame.width,
        frame.height,
        frame.format,
        frame.pitch
    );
    png_out::write_xrgb_png(png, frame.data, frame.width, frame.height, frame.pitch)?;
    println!("wrote {} ({}x{})", png.display(), frame.width, frame.height);
    Ok(())
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "info,webrtc=warn,rtc=warn",
        1 => "debug,webrtc=info,rtc=info",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
