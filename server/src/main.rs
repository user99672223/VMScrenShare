//! vmdesk server binary. See the README for the full setup flow.

#[allow(dead_code)]
mod capture;
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod convert;
#[allow(dead_code)]
mod encoder;
mod png_out;

use std::path::PathBuf;
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
    Setup,
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
        Command::Run => anyhow::bail!("run: not implemented yet"),
        Command::Setup => anyhow::bail!("setup: not implemented yet"),
        Command::Doctor => anyhow::bail!("doctor: not implemented yet"),
        Command::Capture { png, timeout_secs } => capture_png(&config, &png, timeout_secs),
    }
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
}
