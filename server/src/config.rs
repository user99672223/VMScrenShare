//! Server configuration (`/etc/vmdesk/config.toml`).
//!
//! Every field has a default, so a missing file or a partial file works. `setup` writes a
//! fully commented file, preserving values that are already there.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/vmdesk/config.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub video: Video,
    pub network: Network,
    pub capture: Capture,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Video {
    /// Capture/encode rate in frames per second.
    pub fps: u32,
    /// Initial target bitrate in kbit/s (the client can change it at runtime).
    pub bitrate_kbps: u32,
    /// Additional keyframe (IDR) interval in frames inside the encoder. `0` = none (the
    /// time-based interval below and client requests decide).
    pub keyframe_interval: u32,
    /// Send an IDR (with SPS/PPS) at least every this many seconds. `0` = only on request.
    pub keyframe_interval_secs: u32,
    /// Encoder threads (`0` = auto: min(4, cores)).
    pub encoder_threads: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    /// Public IPv4 advertised in ICE candidates. Empty = autodetect from the OCI metadata service.
    pub public_ip: String,
    /// OCI instance metadata endpoint listing the VNICs (contains `publicIp`).
    pub metadata_url: String,
    /// Also bind IPv6 and advertise the interface's global IPv6 address as a host candidate.
    pub ipv6: bool,
    /// IPv6 address to advertise instead of the interface's own global address. Empty = auto.
    pub public_ipv6: String,
    /// First UDP port used for WebRTC media/data (one port per connection, round-robin).
    pub udp_port_min: u16,
    /// Last UDP port (inclusive).
    pub udp_port_max: u16,
    /// HTTP signalling listener. Keep it on loopback and reach it through an SSH tunnel.
    pub signalling_addr: SocketAddr,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Capture {
    /// DRM card node, e.g. `/dev/dri/card0`. Empty = autodetect (driver `vkms`, else by connector).
    pub card: String,
    /// Connector to capture, e.g. `Virtual-2`. Empty = the card's `Virtual-*` connector
    /// (whichever index the kernel assigned).
    pub connector: String,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate_kbps: 12_000,
            keyframe_interval: 0,
            keyframe_interval_secs: 10,
            encoder_threads: 0,
        }
    }
}

impl Default for Network {
    fn default() -> Self {
        Self {
            public_ip: String::new(),
            metadata_url: "http://169.254.169.254/opc/v2/vnics/".into(),
            ipv6: true,
            public_ipv6: String::new(),
            udp_port_min: 50_000,
            udp_port_max: 50_100,
            signalling_addr: "127.0.0.1:8080".parse().unwrap(),
        }
    }
}

impl Config {
    /// Loads the file if it exists, otherwise returns the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let cfg: Config =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                cfg.validate()?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("{} not found, using built-in defaults", path.display());
                Ok(Config::default())
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=120).contains(&self.video.fps),
            "video.fps must be within 1..=120"
        );
        anyhow::ensure!(
            self.video.bitrate_kbps >= 100,
            "video.bitrate_kbps must be at least 100"
        );
        anyhow::ensure!(
            self.network.udp_port_min <= self.network.udp_port_max,
            "network.udp_port_min must be <= udp_port_max"
        );
        if !self.network.public_ip.is_empty() {
            anyhow::ensure!(
                self.network.public_ip.parse::<std::net::Ipv4Addr>().is_ok(),
                "network.public_ip must be an IPv4 address (got {:?})",
                self.network.public_ip
            );
        }
        if !self.network.public_ipv6.is_empty() {
            anyhow::ensure!(
                self.network
                    .public_ipv6
                    .parse::<std::net::Ipv6Addr>()
                    .is_ok(),
                "network.public_ipv6 must be an IPv6 address (got {:?})",
                self.network.public_ipv6
            );
        }
        Ok(())
    }

    /// TOML text with comments, written by `setup`.
    pub fn to_commented_toml(&self) -> String {
        let n = &self.network;
        let v = &self.video;
        let c = &self.capture;
        format!(
            r#"# vmdesk server configuration. Restart the service after editing:
#   sudo systemctl restart vmdesk

[video]
# Capture/encode rate (frames per second).
fps = {fps}
# Initial H.264 target bitrate in kbit/s (the client --bitrate flag overrides it per session).
bitrate_kbps = {bitrate}
# Time-based keyframe (IDR + SPS/PPS) interval in seconds; 0 = only on connect / PLI.
keyframe_interval_secs = {kf_secs}
# Additional encoder-internal keyframe interval in frames; 0 = none.
keyframe_interval = {kf}
# OpenH264 threads; 0 = automatic.
encoder_threads = {threads}

[network]
# Public IPv4 advertised to the client. Empty = read it from the OCI metadata service
# (`sudo ./server setup --public-ip <ip>` writes it here when the metadata has none).
public_ip = "{public_ip}"
metadata_url = "{metadata_url}"
# Also offer the VM's global IPv6 address as a media path (needs an IPv6 security list rule).
ipv6 = {ipv6}
# IPv6 address to advertise instead of the interface's own global address; empty = automatic.
public_ipv6 = "{public_ipv6}"
# UDP ports for WebRTC. Must be allowed in the OCI security list and in iptables/ip6tables.
udp_port_min = {pmin}
udp_port_max = {pmax}
# HTTP signalling listener (loopback only; use `ssh -L 8080:127.0.0.1:8080`).
signalling_addr = "{sig}"

[capture]
# DRM card of the vkms device; empty = autodetect by driver name.
card = "{card}"
# Connector to capture; empty = the card's Virtual-* connector whatever its index.
connector = "{connector}"
"#,
            fps = v.fps,
            bitrate = v.bitrate_kbps,
            kf_secs = v.keyframe_interval_secs,
            kf = v.keyframe_interval,
            threads = v.encoder_threads,
            public_ip = n.public_ip,
            metadata_url = n.metadata_url,
            ipv6 = n.ipv6,
            public_ipv6 = n.public_ipv6,
            pmin = n.udp_port_min,
            pmax = n.udp_port_max,
            sig = n.signalling_addr,
            card = c.card,
            connector = c.connector,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_commented_toml() {
        let cfg = Config::default();
        let text = cfg.to_commented_toml();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);
        parsed.validate().unwrap();
        assert!(parsed.network.ipv6);
        assert!(parsed.capture.connector.is_empty());
    }

    #[test]
    fn values_survive_the_commented_round_trip() {
        let mut cfg = Config::default();
        cfg.network.public_ip = "82.70.62.40".into();
        cfg.network.public_ipv6 = "2001:db8::1".into();
        cfg.network.ipv6 = false;
        cfg.capture.connector = "Virtual-2".into();
        let parsed: Config = toml::from_str(&cfg.to_commented_toml()).unwrap();
        assert_eq!(parsed, cfg);
        parsed.validate().unwrap();
    }

    #[test]
    fn partial_and_older_files_still_parse() {
        let parsed: Config = toml::from_str("[video]\nfps = 60\n").unwrap();
        assert_eq!(parsed.video.fps, 60);
        assert_eq!(parsed.video.bitrate_kbps, 12_000);
        assert_eq!(parsed.network.udp_port_min, 50_000);
        assert_eq!(parsed.capture.connector, "");
        // A file written by the previous version (no ipv6 keys, explicit connector, the old
        // frame-based keyframe interval only).
        let old: Config = toml::from_str(
            "[video]\nkeyframe_interval = 600\n[network]\npublic_ip = \"\"\n[capture]\ncard = \"\"\nconnector = \"Virtual-1\"\n",
        )
        .unwrap();
        assert_eq!(old.capture.connector, "Virtual-1");
        assert!(old.network.ipv6);
        assert_eq!(old.video.keyframe_interval, 600);
        assert_eq!(old.video.keyframe_interval_secs, 10);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[video]\nfsp = 60\n").is_err());
    }

    #[test]
    fn validation() {
        let mut cfg = Config::default();
        cfg.network.udp_port_min = 60_000;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.video.fps = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.network.public_ip = "not-an-ip".into();
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.network.public_ipv6 = "82.70.62.40".into();
        assert!(cfg.validate().is_err());
    }
}
