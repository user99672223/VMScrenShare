//! `server setup`: one-shot provisioning of the VM (run as root).
//!
//! Idempotent: every step checks or overwrites its own file, so it can be re-run after
//! changing the binary or the configuration.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::capture;
use crate::config::Config;

pub const INSTALL_PATH: &str = "/usr/local/bin/vmdesk-server";
pub const XORG_CONF: &str = "/etc/X11/xorg.conf.d/10-vkms.conf";
pub const LIGHTDM_CONF: &str = "/etc/lightdm/lightdm.conf.d/50-vmdesk.conf";
pub const MODULES_CONF: &str = "/etc/modules-load.d/vmdesk.conf";
pub const UDEV_RULE: &str = "/etc/udev/rules.d/70-vmdesk-uinput.rules";
pub const SERVICE_FILE: &str = "/etc/systemd/system/vmdesk.service";
pub const VKMS_BY_PATH: &str = "/dev/dri/by-path/platform-vkms-card";

const APT_PACKAGES: &[&str] = &[
    "xorg",
    "xserver-xorg-input-libinput",
    "xfce4",
    "xfce4-terminal",
    "lightdm",
    "dbus-x11",
    "x11-xserver-utils",
    "iptables",
    "iptables-persistent",
    "libcap2-bin",
];

pub struct SetupOptions {
    /// Desktop user (autologin + service account).
    pub user: String,
    pub config_path: PathBuf,
    pub skip_apt: bool,
    pub install_path: PathBuf,
}

pub fn run(opts: &SetupOptions, config: &Config) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("setup must run as root: sudo ./server setup");
    }
    if !Path::new("/usr/bin/apt-get").exists() {
        bail!("setup expects Ubuntu/Debian (apt-get not found)");
    }
    user_exists(&opts.user)?;

    step("loading kernel modules vkms and uinput");
    let _ = cmd("modprobe", &["vkms"]);
    let _ = cmd("modprobe", &["uinput"]);
    let card = detect_vkms_card(&config.capture)?;
    println!("    vkms card: {card}");

    if opts.skip_apt {
        step("skipping apt (--skip-apt)");
    } else {
        step("installing packages (xorg, xfce4, lightdm, iptables-persistent, ...)");
        install_packages()?;
    }

    step(&format!("writing {XORG_CONF}"));
    write_file(
        Path::new(XORG_CONF),
        &xorg_conf(&card, &config.capture.connector),
    )?;

    step(&format!("writing {LIGHTDM_CONF} (autologin {})", opts.user));
    write_file(Path::new(LIGHTDM_CONF), &lightdm_conf(&opts.user))?;
    cmd("systemctl", &["set-default", "graphical.target"])?;
    cmd("systemctl", &["enable", "lightdm"])?;

    step(&format!("writing {MODULES_CONF}"));
    write_file(Path::new(MODULES_CONF), "vkms\nuinput\n")?;

    step(&format!(
        "writing {UDEV_RULE} (group input may write /dev/uinput)"
    ));
    write_file(Path::new(UDEV_RULE), UDEV_RULE_TEXT)?;
    let _ = cmd("udevadm", &["control", "--reload-rules"]);
    let _ = cmd("udevadm", &["trigger", "--name-match=uinput"]);
    if Path::new("/dev/uinput").exists() {
        let _ = cmd("chgrp", &["input", "/dev/uinput"]);
        let _ = cmd("chmod", &["0660", "/dev/uinput"]);
    }

    step(&format!("adding {} to groups input and video", opts.user));
    cmd("usermod", &["-aG", "input,video", &opts.user])?;

    step(&format!(
        "installing binary to {}",
        opts.install_path.display()
    ));
    install_binary(&opts.install_path)?;
    setcap(&opts.install_path)?;
    if let Ok(exe) = std::env::current_exe() {
        if fs::canonicalize(&exe).ok() != fs::canonicalize(&opts.install_path).ok() {
            if let Err(e) = setcap(&exe) {
                println!("    note: could not setcap {}: {e:#}", exe.display());
            }
        }
    }

    step(&format!(
        "iptables: accept UDP {}-{} and persist",
        config.network.udp_port_min, config.network.udp_port_max
    ));
    firewall(config.network.udp_port_min, config.network.udp_port_max)?;

    step(&format!("writing {}", opts.config_path.display()));
    if opts.config_path.exists() {
        println!("    exists, left untouched");
    } else {
        let mut cfg = config.clone();
        if Path::new(VKMS_BY_PATH).exists() {
            cfg.capture.card = VKMS_BY_PATH.to_string();
        }
        write_file(&opts.config_path, &cfg.to_commented_toml())?;
    }

    step(&format!(
        "disabling screen blanking for {}'s XFCE session",
        opts.user
    ));
    if let Err(e) = xfce_no_blank(&opts.user) {
        println!("    warning: {e:#}");
    }

    step(&format!("installing systemd service {SERVICE_FILE}"));
    write_file(
        Path::new(SERVICE_FILE),
        &service_unit(&opts.user, &opts.install_path, &opts.config_path),
    )?;
    cmd("systemctl", &["daemon-reload"])?;
    cmd("systemctl", &["enable", "vmdesk"])?;
    cmd("systemctl", &["restart", "vmdesk"])?;

    println!();
    println!("Setup complete. Next:");
    println!("  1. Reboot so lightdm/XFCE start on vkms:   sudo reboot");
    println!(
        "  2. OCI console -> VCN -> security list: ingress rule for UDP {}-{} from your IP (or 0.0.0.0/0).",
        config.network.udp_port_min, config.network.udp_port_max
    );
    println!("  3. After the reboot check everything:      ./server doctor");
    println!("  4. From your laptop:                        ssh -N -L 8080:127.0.0.1:8080 ubuntu@<vm-ip>");
    println!("     then run the client.");
    Ok(())
}

fn step(text: &str) {
    println!("==> {text}");
}

fn cmd(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .with_context(|| format!("running {program} {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn user_exists(user: &str) -> Result<()> {
    let passwd = fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?;
    if passwd.lines().any(|l| l.split(':').next() == Some(user)) {
        Ok(())
    } else {
        bail!("user {user} does not exist (use --user <name>)")
    }
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn detect_vkms_card(capture_cfg: &crate::config::Capture) -> Result<String> {
    if Path::new(VKMS_BY_PATH).exists() {
        return Ok(VKMS_BY_PATH.to_string());
    }
    let card = capture::find_card(&capture_cfg.card, &capture_cfg.connector)
        .context("vkms card not found; is the module loaded? (modprobe vkms)")?;
    Ok(card.path().display().to_string())
}

fn install_packages() -> Result<()> {
    // Preseed so iptables-persistent does not prompt (and saves the current rules).
    let mut child = Command::new("debconf-set-selections")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("running debconf-set-selections")?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(
            b"iptables-persistent iptables-persistent/autosave_v4 boolean true\n\
              iptables-persistent iptables-persistent/autosave_v6 boolean true\n",
        )?;
    }
    let _ = child.wait();

    cmd("apt-get", &["update"])?;
    let mut args = vec!["install", "-y"];
    args.extend(APT_PACKAGES);
    cmd("apt-get", &args)?;
    Ok(())
}

fn install_binary(dest: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locating this binary")?;
    if fs::canonicalize(&exe).ok() == fs::canonicalize(dest).ok() {
        println!("    already running from {}", dest.display());
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("tmp");
    fs::copy(&exe, &tmp)
        .with_context(|| format!("copying {} to {}", exe.display(), tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, dest).with_context(|| format!("installing {}", dest.display()))?;
    Ok(())
}

fn setcap(path: &Path) -> Result<()> {
    let p = path.to_string_lossy();
    cmd("setcap", &["cap_sys_admin+ep", &p]).map(|_| ())
}

fn firewall(min: u16, max: u16) -> Result<()> {
    let ports = format!("{min}:{max}");
    let rule = ["-p", "udp", "-m", "udp", "--dport", &ports, "-j", "ACCEPT"];
    let mut check = vec!["-C", "INPUT"];
    check.extend(rule);
    if cmd("iptables", &check).is_ok() {
        println!("    rule already present");
    } else {
        let mut insert = vec!["-I", "INPUT", "1"];
        insert.extend(rule);
        cmd("iptables", &insert)?;
        println!("    rule inserted at the top of INPUT");
    }
    if Path::new("/usr/sbin/netfilter-persistent").exists() {
        cmd("netfilter-persistent", &["save"])?;
        println!("    saved with netfilter-persistent");
    } else {
        let rules = cmd("iptables-save", &[])?;
        fs::create_dir_all("/etc/iptables")?;
        fs::write("/etc/iptables/rules.v4", rules)?;
        println!(
            "    saved to /etc/iptables/rules.v4 (install iptables-persistent to restore at boot)"
        );
    }
    Ok(())
}

fn home_dir(user: &str) -> Result<PathBuf> {
    let passwd = fs::read_to_string("/etc/passwd")?;
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.first() == Some(&user) && fields.len() >= 6 {
            return Ok(PathBuf::from(fields[5]));
        }
    }
    bail!("no home directory for {user}")
}

fn xfce_no_blank(user: &str) -> Result<()> {
    let home = home_dir(user)?;
    let files: [(PathBuf, &str); 3] = [
        (
            home.join(".config/autostart/vmdesk-noblank.desktop"),
            AUTOSTART_NOBLANK,
        ),
        (
            home.join(".config/xfce4/xfconf/xfce-perchannel-xml/xfce4-power-manager.xml"),
            XFCE_POWER_MANAGER_XML,
        ),
        (
            home.join(".config/xfce4/xfconf/xfce-perchannel-xml/xfce4-screensaver.xml"),
            XFCE_SCREENSAVER_XML,
        ),
    ];
    for (path, content) in files {
        if path.exists() && !path.ends_with("vmdesk-noblank.desktop") {
            println!("    {} exists, left untouched", path.display());
            continue;
        }
        write_file(&path, content)?;
    }
    cmd(
        "chown",
        &[
            "-R",
            &format!("{user}:{user}"),
            &home.join(".config").to_string_lossy(),
        ],
    )?;
    Ok(())
}

pub fn xorg_conf(kmsdev: &str, connector: &str) -> String {
    format!(
        r#"# Generated by `server setup` (vmdesk). Xorg on the vkms virtual display.
Section "Device"
    Identifier "vkms"
    Driver     "modesetting"
    Option     "kmsdev"      "{kmsdev}"
    Option     "AccelMethod" "none"
EndSection

Section "Monitor"
    Identifier "{connector}"
    Option     "PreferredMode" "1920x1080"
    Option     "DPMS" "false"
EndSection

Section "Screen"
    Identifier   "Screen0"
    Device       "vkms"
    Monitor      "{connector}"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1920x1080"
    EndSubSection
EndSection

Section "ServerLayout"
    Identifier "Layout0"
    Screen     "Screen0"
    Option     "BlankTime"   "0"
    Option     "StandbyTime" "0"
    Option     "SuspendTime" "0"
    Option     "OffTime"     "0"
EndSection

Section "ServerFlags"
    Option "AutoAddGPU" "false"
EndSection
"#
    )
}

pub fn lightdm_conf(user: &str) -> String {
    format!(
        r#"# Generated by `server setup` (vmdesk).
[Seat:*]
autologin-user={user}
autologin-user-timeout=0
autologin-session=xfce
user-session=xfce
"#
    )
}

pub const UDEV_RULE_TEXT: &str = r#"# Generated by `server setup` (vmdesk): let group input create uinput devices.
KERNEL=="uinput", SUBSYSTEM=="misc", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
"#;

pub fn service_unit(user: &str, binary: &Path, config: &Path) -> String {
    format!(
        r#"# Generated by `server setup` (vmdesk).
[Unit]
Description=vmdesk remote desktop server
After=network-online.target lightdm.service
Wants=network-online.target

[Service]
User={user}
SupplementaryGroups=input video
AmbientCapabilities=CAP_SYS_ADMIN
ExecStart={binary} run --config {config}
Restart=always
RestartSec=2
Environment=RUST_LOG=info,webrtc=warn,rtc=warn

[Install]
WantedBy=multi-user.target
"#,
        binary = binary.display(),
        config = config.display()
    )
}

const AUTOSTART_NOBLANK: &str = r#"[Desktop Entry]
Type=Application
Name=vmdesk: disable screen blanking
Exec=sh -c 'xset s off; xset s noblank; xset -dpms'
OnlyShowIn=XFCE;
X-GNOME-Autostart-enabled=true
"#;

const XFCE_POWER_MANAGER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-power-manager" version="1.0">
  <property name="xfce4-power-manager" type="empty">
    <property name="dpms-enabled" type="bool" value="false"/>
    <property name="blank-on-ac" type="int" value="0"/>
    <property name="blank-on-battery" type="int" value="0"/>
    <property name="dpms-on-ac-sleep" type="uint" value="0"/>
    <property name="dpms-on-ac-off" type="uint" value="0"/>
    <property name="lock-screen-suspend-hibernate" type="bool" value="false"/>
    <property name="logind-handle-lid-switch" type="bool" value="false"/>
  </property>
</channel>
"#;

const XFCE_SCREENSAVER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-screensaver" version="1.0">
  <property name="saver" type="empty">
    <property name="enabled" type="bool" value="false"/>
    <property name="idle-activation" type="empty">
      <property name="enabled" type="bool" value="false"/>
    </property>
  </property>
  <property name="lock" type="empty">
    <property name="enabled" type="bool" value="false"/>
  </property>
</channel>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_contain_the_moving_parts() {
        let x = xorg_conf("/dev/dri/card0", "Virtual-1");
        assert!(x.contains("Option     \"kmsdev\"      \"/dev/dri/card0\""));
        assert!(x.contains("Identifier \"Virtual-1\""));
        assert!(x.contains("\"AccelMethod\" \"none\""));
        assert!(x.contains("PreferredMode\" \"1920x1080\""));
        let l = lightdm_conf("ubuntu");
        assert!(l.contains("autologin-user=ubuntu"));
        let s = service_unit(
            "ubuntu",
            Path::new("/usr/local/bin/vmdesk-server"),
            Path::new("/etc/vmdesk/config.toml"),
        );
        assert!(s.contains(
            "ExecStart=/usr/local/bin/vmdesk-server run --config /etc/vmdesk/config.toml"
        ));
        assert!(s.contains("User=ubuntu"));
        assert!(s.contains("AmbientCapabilities=CAP_SYS_ADMIN"));
        assert!(UDEV_RULE_TEXT.contains("GROUP=\"input\""));
    }
}
