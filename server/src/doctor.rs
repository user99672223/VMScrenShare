//! `server doctor`: checks the VM and prints one PASS/FAIL/WARN line per item with the fix.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture;
use crate::config::Config;
use crate::convert::{self, I420Frame, XrgbImage};
use crate::encoder::{self, EncoderSettings};
use crate::{netinfo, setup, testpattern};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

struct Check {
    status: Status,
    name: &'static str,
    detail: String,
    fix: Option<String>,
}

fn pass(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        status: Status::Pass,
        name,
        detail: detail.into(),
        fix: None,
    }
}

fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        status: Status::Warn,
        name,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        status: Status::Fail,
        name,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

/// Runs all checks. Returns `Ok(true)` when nothing failed.
pub fn run(config: &Config) -> Result<bool> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "./server".into());
    let mut checks = Vec::new();

    checks.push(check_vkms_module());
    let card = check_vkms_card(config, &mut checks);
    if let Some(card) = card {
        check_display_and_capture(card, &config.capture.connector, &mut checks);
    }
    checks.push(check_capability(&exe));
    checks.push(check_uinput());
    checks.push(check_groups());
    checks.push(check_xorg_conf());
    checks.push(check_public_ipv4(config));
    checks.push(check_ipv6(config));
    checks.push(check_firewall("iptables", config));
    if config.network.ipv6 {
        checks.push(check_firewall("ip6tables", config));
    }
    checks.push(check_signalling_port(config));
    checks.push(check_service());
    checks.push(check_convert());
    checks.push(check_encoder(config));

    let mut failures = 0;
    for c in &checks {
        let tag = match c.status {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => {
                failures += 1;
                "FAIL"
            }
        };
        println!("{tag}  {:<22} {}", c.name, c.detail);
        if let Some(fix) = &c.fix {
            for line in fix.lines() {
                println!("      fix: {line}");
            }
        }
    }
    println!();
    if failures == 0 {
        println!("All checks passed.");
    } else {
        println!("{failures} check(s) failed.");
    }
    Ok(failures == 0)
}

fn check_vkms_module() -> Check {
    if Path::new("/sys/module/vkms").exists() {
        pass("vkms module", "loaded")
    } else {
        fail(
            "vkms module",
            "not loaded",
            "sudo modprobe vkms   (persisted by `sudo ./server setup` via /etc/modules-load.d/vmdesk.conf)",
        )
    }
}

fn check_vkms_card(config: &Config, checks: &mut Vec<Check>) -> Option<capture::Card> {
    match capture::find_card(&config.capture.card, &config.capture.connector) {
        Ok(card) => {
            let driver = card.driver_name().unwrap_or_else(|_| "?".into());
            let connectors = card.connector_names().unwrap_or_default();
            checks.push(pass(
                "vkms card",
                format!(
                    "{} (driver {driver}, connectors {connectors:?})",
                    card.path().display()
                ),
            ));
            Some(card)
        }
        Err(e) => {
            checks.push(fail(
                "vkms card",
                format!("{e:#}"),
                "sudo modprobe vkms; if another DRM device is present set capture.card in the config",
            ));
            None
        }
    }
}

fn check_display_and_capture(card: capture::Card, connector_spec: &str, checks: &mut Vec<Check>) {
    match capture::locate_display(&card, connector_spec) {
        Ok(display) => {
            checks.push(pass(
                "Xorg on vkms",
                format!(
                    "{} active, {}x{}@{}Hz{}",
                    display.name,
                    display.width,
                    display.height,
                    display.refresh,
                    if connector_spec.is_empty() {
                        " (connector selected by type)"
                    } else {
                        ""
                    }
                ),
            ));
            let mut source = capture::FrameSource::new(card, display);
            let t = Instant::now();
            match source.capture() {
                Ok(frame) => checks.push(pass(
                    "framebuffer capture",
                    format!(
                        "{}x{} {:?} pitch {} in {:.1} ms",
                        frame.width,
                        frame.height,
                        frame.format,
                        frame.pitch,
                        t.elapsed().as_secs_f64() * 1000.0
                    ),
                )),
                Err(e) => checks.push(fail(
                    "framebuffer capture",
                    format!("{e:#}"),
                    "GETFB2 needs CAP_SYS_ADMIN: sudo setcap cap_sys_admin+ep <this binary>, or run `sudo ./server setup`",
                )),
            }
        }
        Err(e) => {
            let fix = if connector_spec.is_empty() {
                "Is the desktop up? systemctl status lightdm; journalctl -u lightdm -b; grep -E '\\(EE\\)|vkms' /var/log/Xorg.0.log\n\
                 After `sudo ./server setup` a reboot is needed once."
                    .to_string()
            } else {
                format!(
                    "capture.connector = \"{connector_spec}\" is set in the config; use \"\" to pick the vkms Virtual-* connector automatically.\n\
                     Also: systemctl status lightdm; journalctl -u lightdm -b; grep -E '\\(EE\\)|vkms' /var/log/Xorg.0.log"
                )
            };
            checks.push(fail("Xorg on vkms", format!("{e:#}"), fix));
        }
    }
}

const CAP_SYS_ADMIN_BIT: u32 = 21;

fn check_capability(exe: &str) -> Check {
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        return pass("CAP_SYS_ADMIN", "running as root");
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let cap_eff = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .unwrap_or(0);
    if cap_eff & (1u64 << CAP_SYS_ADMIN_BIT) != 0 {
        pass("CAP_SYS_ADMIN", "effective (file capability)")
    } else {
        fail(
            "CAP_SYS_ADMIN",
            "not in the effective capability set",
            format!("sudo setcap cap_sys_admin+ep {exe}   (the systemd service gets it via AmbientCapabilities)"),
        )
    }
}

fn check_uinput() -> Check {
    let path = Path::new("/dev/uinput");
    if !path.exists() {
        return fail(
            "/dev/uinput",
            "missing",
            "sudo modprobe uinput   (persisted by setup via /etc/modules-load.d/vmdesk.conf)",
        );
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => pass("/dev/uinput", "writable"),
        Err(e) => fail(
            "/dev/uinput",
            format!("not writable: {e}"),
            format!(
                "sudo ./server setup installs {} and adds you to group input; re-login afterwards.\n\
                 Quick fix: sudo chgrp input /dev/uinput && sudo chmod 660 /dev/uinput",
                setup::UDEV_RULE
            ),
        ),
    }
}

fn check_groups() -> Check {
    if unsafe { libc::geteuid() } == 0 {
        return pass("groups", "root (group membership not needed)");
    }
    let user = current_user().unwrap_or_default();
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let member = |group: &str| {
        groups.lines().any(|l| {
            let f: Vec<&str> = l.split(':').collect();
            f.first() == Some(&group)
                && f.get(3)
                    .map(|m| m.split(',').any(|u| u == user))
                    .unwrap_or(false)
        })
    };
    let missing: Vec<&str> = ["input", "video"]
        .into_iter()
        .filter(|g| !member(g))
        .collect();
    if missing.is_empty() {
        pass("groups", format!("{user} is in input and video"))
    } else {
        warn(
            "groups",
            format!("{user} is not in {}", missing.join(", ")),
            format!("sudo usermod -aG input,video {user}; then log out and in again"),
        )
    }
}

fn current_user() -> Option<String> {
    std::env::var("USER").ok().or_else(|| {
        std::process::Command::new("id")
            .arg("-un")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    })
}

fn check_xorg_conf() -> Check {
    if Path::new(setup::XORG_CONF).exists() {
        pass("xorg.conf.d", format!("{} present", setup::XORG_CONF))
    } else {
        warn(
            "xorg.conf.d",
            format!("{} missing", setup::XORG_CONF),
            "sudo ./server setup",
        )
    }
}

fn check_public_ipv4(config: &Config) -> Check {
    if !config.network.public_ip.is_empty() {
        return pass(
            "public IPv4",
            format!("{} (config)", config.network.public_ip),
        );
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail("public IPv4", format!("runtime error: {e}"), "retry"),
    };
    match rt.block_on(crate::metadata::detect_public_ip(&config.network.metadata_url)) {
        Ok(ip) => pass("public IPv4", format!("{ip} (OCI metadata)")),
        Err(e) => fail(
            "public IPv4",
            format!("{e:#}"),
            "sudo ./server setup --skip-apt --public-ip <VM public IPv4>   (writes network.public_ip and restarts the service)",
        ),
    }
}

fn check_ipv6(config: &Config) -> Check {
    if !config.network.ipv6 {
        return pass("IPv6", "disabled in config (IPv4 only)");
    }
    if !config.network.public_ipv6.is_empty() {
        return pass(
            "IPv6",
            format!("{} (config override)", config.network.public_ipv6),
        );
    }
    match netinfo::global_ipv6() {
        Some(ip) => pass("IPv6", format!("{ip} (interface, advertised as a host candidate)")),
        None => warn(
            "IPv6",
            "no global IPv6 address on any interface; IPv4 only",
            "fine if the VM has no IPv6; otherwise check the VNIC's IPv6 assignment, or set network.ipv6 = false",
        ),
    }
}

/// Checks the INPUT chain of `tool` (`iptables` or `ip6tables`) for the UDP ACCEPT rule.
fn check_firewall(tool: &'static str, config: &Config) -> Check {
    let ports = format!(
        "{}:{}",
        config.network.udp_port_min, config.network.udp_port_max
    );
    let fix = format!(
        "sudo {tool} -I INPUT 1 -p udp -m udp --dport {ports} -j ACCEPT && sudo netfilter-persistent save"
    );
    let output = if unsafe { libc::geteuid() } == 0 {
        std::process::Command::new(tool)
            .args(["-S", "INPUT"])
            .output()
    } else {
        std::process::Command::new("sudo")
            .args(["-n", tool, "-S", "INPUT"])
            .output()
    };
    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            return warn(
                tool,
                format!(
                    "cannot list rules ({})",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                "run `sudo ./server doctor` to check the firewall",
            )
        }
        Err(e) => return warn(tool, format!("{tool} not runnable: {e}"), fix),
    };
    let mut accept_at = None;
    let mut reject_at = None;
    for (i, line) in output.lines().enumerate() {
        if line.contains("-p udp")
            && line.contains(&format!("--dport {ports}"))
            && line.contains("-j ACCEPT")
        {
            accept_at.get_or_insert(i);
        }
        if (line.contains("-j REJECT") || line.contains("-j DROP")) && !line.contains("--dport") {
            reject_at.get_or_insert(i);
        }
    }
    match (accept_at, reject_at) {
        (Some(a), Some(r)) if a > r => fail(
            tool,
            format!("UDP {ports} ACCEPT rule comes after a REJECT/DROP rule"),
            fix,
        ),
        (Some(_), _) => pass(tool, format!("UDP {ports} accepted")),
        (None, _) => fail(tool, format!("no ACCEPT rule for UDP {ports}"), fix),
    }
}

fn check_signalling_port(config: &Config) -> Check {
    let addr = config.network.signalling_addr;
    match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Err(_) => pass(
            "signalling port",
            format!("{addr} free (server not running)"),
        ),
        Ok(mut stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let req = format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                proto::signalling::HEALTH_PATH,
                addr
            );
            let mut body = String::new();
            if stream.write_all(req.as_bytes()).is_ok() {
                let _ = stream.read_to_string(&mut body);
            }
            if body.contains(proto::signalling::HEALTH_BODY) && body.starts_with("HTTP/1.1 200") {
                pass(
                    "signalling port",
                    format!("{addr}: vmdesk server is running"),
                )
            } else {
                fail(
                    "signalling port",
                    format!("{addr} is used by another program"),
                    "stop that program or change network.signalling_addr (and the ssh -L port)",
                )
            }
        }
    }
}

fn check_service() -> Check {
    let state = |arg: &str| {
        std::process::Command::new("systemctl")
            .args([arg, "vmdesk"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    let active = state("is-active");
    let enabled = state("is-enabled");
    let detail = format!("active: {active}, enabled: {enabled}");
    if active == "active" && enabled == "enabled" {
        pass("systemd service", detail)
    } else {
        warn(
            "systemd service",
            detail,
            "sudo ./server setup   or   sudo systemctl enable --now vmdesk; journalctl -u vmdesk -b",
        )
    }
}

/// Frames pushed through the encoder by the self-test (about a second of video).
const ENCODER_TEST_FRAMES: usize = 30;

/// Copy + convert speed for a 1080p frame: must stay far below the frame time.
fn check_convert() -> Check {
    let (w, h) = (1920usize, 1080usize);
    let pitch = w * 4 + 256; // vkms pitches are not always tight
    let src = testpattern::xrgb(w, h, pitch, 0);
    let mut copy = XrgbImage::default();
    let mut frame = I420Frame::new(0, 0);
    // Warm up (page in, spawn the rayon pool), then time a few iterations.
    convert::copy_xrgb(&src, w, h, pitch, &mut copy);
    convert::xrgb_to_i420(&copy.data, w, h, copy.pitch(), &mut frame);
    let iterations = 10;
    let mut copy_total = Duration::ZERO;
    let mut convert_total = Duration::ZERO;
    for _ in 0..iterations {
        let t = Instant::now();
        convert::copy_xrgb(&src, w, h, pitch, &mut copy);
        copy_total += t.elapsed();
        let t = Instant::now();
        convert::xrgb_to_i420(&copy.data, w, h, copy.pitch(), &mut frame);
        convert_total += t.elapsed();
    }
    let copy_ms = copy_total.as_secs_f64() * 1000.0 / iterations as f64;
    let convert_ms = convert_total.as_secs_f64() * 1000.0 / iterations as f64;
    let detail = format!(
        "1080p copy {copy_ms:.1} ms + XRGB->I420 {convert_ms:.1} ms ({} kernel, {} threads)",
        convert::kernel_name(),
        rayon::current_num_threads()
    );
    if copy_ms + convert_ms < 12.0 {
        pass("frame conversion", detail)
    } else {
        warn(
            "frame conversion",
            detail,
            "conversion is slow; check that the VM is not CPU starved (top) and report the numbers",
        )
    }
}

/// Encodes a second of moving 1080p video and reports the sustained frame rate.
fn check_encoder(config: &Config) -> Check {
    let settings = EncoderSettings {
        width: 1920,
        height: 1080,
        fps: config.video.fps.max(1),
        bitrate_kbps: config.video.bitrate_kbps,
        keyframe_interval: config.video.keyframe_interval,
        threads: config.video.encoder_threads,
    };
    let mut enc = match encoder::create(settings) {
        Ok(e) => e,
        Err(e) => {
            return fail(
                "encoder",
                format!("{e:#}"),
                "report this: OpenH264 failed to initialise",
            )
        }
    };
    let frames: Vec<I420Frame> = (0..ENCODER_TEST_FRAMES)
        .map(|i| testpattern::i420(1920, 1080, i))
        .collect();
    let mut idr_bytes = 0usize;
    let mut total_bytes = 0usize;
    let mut max_ms = 0.0f64;
    let t = Instant::now();
    for (i, frame) in frames.iter().enumerate() {
        let t_frame = Instant::now();
        match enc.encode(frame, (i as u64) * 1000 / u64::from(settings.fps), i == 0) {
            Ok(Some(out)) => {
                if i == 0 {
                    if !out.keyframe {
                        return fail("encoder", "first frame is not an IDR", "report this");
                    }
                    idr_bytes = out.data.len();
                }
                total_bytes += out.data.len();
            }
            Ok(None) => return fail("encoder", format!("frame {i} skipped"), "report this"),
            Err(e) => return fail("encoder", format!("{e:#}"), "report this"),
        }
        max_ms = max_ms.max(t_frame.elapsed().as_secs_f64() * 1000.0);
    }
    let secs = t.elapsed().as_secs_f64().max(1e-6);
    let fps = ENCODER_TEST_FRAMES as f64 / secs;
    let detail = format!(
        "OpenH264 1080p: {fps:.0} fps sustained ({:.1} ms/frame avg, {max_ms:.0} ms max), IDR {idr_bytes} bytes, {:.0} kbit/s at {} fps target",
        secs * 1000.0 / ENCODER_TEST_FRAMES as f64,
        total_bytes as f64 * 8.0 / 1000.0 / (ENCODER_TEST_FRAMES as f64 / f64::from(settings.fps)),
        settings.fps
    );
    if fps >= f64::from(settings.fps) {
        pass("encoder", detail)
    } else if fps >= f64::from(settings.fps) * 0.66 {
        warn(
            "encoder",
            detail,
            "the encoder cannot keep up with video.fps; lower video.fps or set video.encoder_threads = 4",
        )
    } else {
        fail(
            "encoder",
            detail,
            "far below the configured video.fps: check CPU load (top), lower video.fps or the resolution",
        )
    }
}
