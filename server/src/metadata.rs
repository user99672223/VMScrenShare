//! Public IP discovery through the OCI instance metadata service.
//!
//! `GET http://169.254.169.254/opc/v2/vnics/` with `Authorization: Bearer Oracle` returns a JSON
//! array of VNICs; the primary VNIC's `publicIp` is the address the client must dial.

use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Extracts the first non-empty `publicIp` from the VNIC listing.
pub fn parse_public_ip(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let items: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(a) => a.iter().collect(),
        obj @ serde_json::Value::Object(_) => vec![obj],
        _ => return None,
    };
    items
        .iter()
        .filter_map(|v| v.get("publicIp").and_then(|ip| ip.as_str()))
        .map(str::trim)
        .find(|ip| !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok())
        .map(str::to_owned)
}

/// Queries the metadata service once (3 s timeout).
pub async fn detect_public_ip(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .context("building HTTP client")?;
    let response = client
        .get(url)
        .header("Authorization", "Bearer Oracle")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    let body = response.text().await.context("reading metadata body")?;
    if !status.is_success() {
        bail!("metadata service returned {status}: {}", body.trim());
    }
    match parse_public_ip(&body) {
        Some(ip) => Ok(ip),
        None => bail!("no publicIp in metadata response: {}", body.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oci_vnic_listing() {
        let json = r#"[{"vnicId":"ocid1.vnic.oc1..x","privateIp":"10.0.0.7","vlanTag":1,
            "macAddr":"02:00:17:00:00:01","virtualRouterIp":"10.0.0.1","subnetCidrBlock":"10.0.0.0/24",
            "nicIndex":0,"publicIp":"129.146.10.20"}]"#;
        assert_eq!(parse_public_ip(json), Some("129.146.10.20".to_string()));
    }

    #[test]
    fn skips_vnics_without_public_ip() {
        let json = r#"[{"privateIp":"10.0.0.7"},{"privateIp":"10.0.1.7","publicIp":""},
            {"privateIp":"10.0.2.7","publicIp":" 203.0.113.5 "}]"#;
        assert_eq!(parse_public_ip(json), Some("203.0.113.5".to_string()));
        assert_eq!(parse_public_ip(r#"[{"privateIp":"10.0.0.7"}]"#), None);
        assert_eq!(parse_public_ip("not json"), None);
        assert_eq!(parse_public_ip(r#"[{"publicIp":"garbage"}]"#), None);
    }
}
