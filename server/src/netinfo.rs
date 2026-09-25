//! Local network facts: the VM's global IPv6 address.

use std::net::{IpAddr, Ipv6Addr};

/// The first globally routable IPv6 address configured on a non-loopback interface
/// (skips link-local `fe80::/10`, unique-local `fc00::/7`, loopback and multicast).
pub fn global_ipv6() -> Option<Ipv6Addr> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    let mut found: Vec<Ipv6Addr> = interfaces
        .iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V6(v6) if is_global(&v6) => Some(v6),
            _ => None,
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found.into_iter().next()
}

/// Approximation of "globally routable": the `2000::/3` unicast range.
pub fn is_global(addr: &Ipv6Addr) -> bool {
    let first = addr.segments()[0];
    (0x2000..0x4000).contains(&first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_range_only() {
        assert!(is_global(&"2603:c020:4000:1::5".parse().unwrap()));
        assert!(is_global(&"2a01:4f8::1".parse().unwrap()));
        assert!(!is_global(&"fe80::1".parse().unwrap()));
        assert!(!is_global(&"fd00::1".parse().unwrap()));
        assert!(!is_global(&"::1".parse().unwrap()));
        assert!(!is_global(&"ff02::1".parse().unwrap()));
    }

    #[test]
    fn enumeration_does_not_panic() {
        let _ = global_ipv6();
    }
}
