//! `server doctor`: checks the VM and prints one PASS/FAIL/WARN line per item with the fix.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture;
use crate::config::Config;
use crate::convert::I420Frame;
use crate::encoder::{self, EncoderSettings};
use crate::setup;

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
    checks.push(check_public_ip(config));
    checks.push(check_iptables(config));
    checks.push(check_signalling_port(config));
    checks.push(check_service());
    checks.push(check_encoder());

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

fn check_display_and_capture(card: capture::Card, connector: &str, checks: &mut Vec<Check>) {
    match capture::locate_display(&card, connector) {
        Ok(display) => {
            checks.push(pass(
                "Xorg on vkms",
                format!(
                    "{connector} active, {}x{}@{}Hz",
                    display.width, display.height, display.refresh
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
            checks.push(fail(
                "Xorg on vkms",
                format!("{e:#}"),
                "Is the desktop up? systemctl status lightdm; journalctl -u lightdm -b; grep -E '\\(EE\\)|vkms' /var/log/Xorg.0.log\n\
                 After `sudo ./server setup` a reboot is needed once.",
            ));
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

fn check_public_ip(config: &Config) -> Check {
    if !config.network.public_ip.is_empty() {
        return pass(
            "public IP",
            format!("{} (config)", config.network.public_ip),
        );
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail("public IP", format!("runtime error: {e}"), "retry"),
    };
    match rt.block_on(crate::metadata::detect_public_ip(&config.network.metadata_url)) {
        Ok(ip) => pass("public IP", format!("{ip} (OCI metadata)")),
        Err(e) => fail(
            "public IP",
            format!("{e:#}"),
            "set network.public_ip = \"<VM public IP>\" in /etc/vmdesk/config.toml and restart vmdesk",
        ),
    }
}

fn check_iptables(config: &Config) -> Check {
    let ports = format!(
        "{}:{}",
        config.network.udp_port_min, config.network.udp_port_max
    );
    let fix = format!(
        "sudo iptables -I INPUT 1 -p udp -m udp --dport {ports} -j ACCEPT && sudo netfilter-persistent save"
    );
    let output = if unsafe { libc::geteuid() } == 0 {
        std::process::Command::new("iptables")
            .args(["-S", "INPUT"])
            .output()
    } else {
        std::process::Command::new("sudo")
            .args(["-n", "iptables", "-S", "INPUT"])
            .output()
    };
    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            return warn(
                "iptables",
                format!(
                    "cannot list rules ({})",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                "run `sudo ./server doctor` to check the firewall",
            )
        }
        Err(e) => return warn("iptables", format!("iptables not runnable: {e}"), fix),
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
            "iptables",
            format!("UDP {ports} ACCEPT rule comes after a REJECT/DROP rule"),
            fix,
        ),
        (Some(_), _) => pass("iptables", format!("UDP {ports} accepted")),
        (None, _) => fail("iptables", format!("no ACCEPT rule for UDP {ports}"), fix),
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

fn check_encoder() -> Check {
    let settings = EncoderSettings {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate_kbps: 12_000,
        keyframe_interval: 600,
        threads: 0,
    };
    let t = Instant::now();
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
    let frame = I420Frame::new(1920, 1080);
    let mut detail = String::new();
    match enc.encode(&frame, 0, true) {
        Ok(Some(out)) => {
            let _ = write!(
                detail,
                "OpenH264 1080p keyframe {} bytes in {:.0} ms",
                out.data.len(),
                t.elapsed().as_secs_f64() * 1000.0
            );
            pass("encoder", detail)
        }
        Ok(None) => fail("encoder", "no output for a forced keyframe", "report this"),
        Err(e) => fail("encoder", format!("{e:#}"), "report this"),
    }
}
