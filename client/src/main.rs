//! vmdesk client binary.
//!
//! Threads:
//! * main: winit event loop, softbuffer presentation, input capture ([`app`]).
//! * `vmdesk-net`: tokio runtime running the WebRTC session ([`net`]).
//! * `vmdesk-decode`: FFmpeg decoding + swscale scaling ([`decoder`]).

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use client::{app, decoder, net};
use winit::event_loop::EventLoop;

#[derive(Parser, Debug)]
#[command(name = "client", version, about = "vmdesk remote desktop client")]
struct Cli {
    /// Signalling URL of the server (normally reached through `ssh -L 8080:127.0.0.1:8080 <vm>`).
    #[arg(long, default_value = proto::signalling::DEFAULT_SERVER_URL)]
    server: String,

    /// Video bitrate to request from the server, in kbit/s (default: the server's config).
    #[arg(long)]
    bitrate: Option<u32>,

    /// Disable hardware decoding (D3D11VA / VA-API) and use FFmpeg's software h264 decoder.
    #[arg(long)]
    no_hwdec: bool,

    /// Increase log verbosity (-v: debug, -vv: trace). RUST_LOG overrides this.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let event_loop = EventLoop::<app::UserEvent>::with_user_event()
        .build()
        .context("creating event loop")?;
    let proxy = event_loop.create_proxy();
    let view = Arc::new(app::SharedView::new());

    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<net::UiCommand>();
    let (au_tx, au_rx) = std::sync::mpsc::sync_channel::<bytes::Bytes>(32);
    let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel::<net::DecoderRequest>();

    let hw = if cli.no_hwdec {
        decoder::HwPreference::Software
    } else {
        decoder::HwPreference::Auto
    };
    {
        let view = Arc::clone(&view);
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name("vmdesk-decode".into())
            .spawn(move || decoder::run(au_rx, view, proxy, req_tx, hw))
            .context("spawning decoder thread")?;
    }
    let net_thread = {
        let cfg = net::NetConfig {
            server_url: cli.server.clone(),
            bitrate_kbps: cli.bitrate,
        };
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name("vmdesk-net".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = proxy.send_event(app::UserEvent::Fatal(format!(
                            "cannot start network runtime: {e}"
                        )));
                        return;
                    }
                };
                runtime.block_on(async move {
                    if let Err(e) = net::run(cfg, ui_rx, au_tx, req_rx, proxy.clone()).await {
                        let _ = proxy.send_event(app::UserEvent::Fatal(format!("{e:#}")));
                    }
                });
            })
            .context("spawning network thread")?
    };

    let result = app::run(event_loop, ui_tx, view);
    // Let the network thread deliver ReleaseAll and close the connection before exiting.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while !net_thread.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    result
}

fn init_logging(verbose: u8) {
    proto::logging::init(verbose, vec!["client", "proto"]);
}
