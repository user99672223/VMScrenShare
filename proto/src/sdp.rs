//! Minimal SDP inspection and rewriting.
//!
//! The server is ICE-lite. Its IPv4 sits behind OCI's 1:1 NAT, so the IPv4 host candidates it
//! gathers carry the VM's private address and have to be rewritten to the public one before the
//! answer is returned ([`rewrite_host_candidates`]). IPv6 addresses are global (no NAT) and are
//! kept unless an override is configured. These helpers work on the SDP text only.

/// Returns true if the SDP has an `m=<kind>` media section (`"video"`, `"audio"`, `"application"`).
pub fn has_media(sdp: &str, kind: &str) -> bool {
    let prefix = format!("m={kind} ");
    lines(sdp).any(|l| l.starts_with(&prefix))
}

/// Number of `a=candidate:` lines.
pub fn candidate_count(sdp: &str) -> usize {
    lines(sdp).filter(|l| l.starts_with("a=candidate:")).count()
}

/// True if the session advertises `a=ice-lite`.
pub fn is_ice_lite(sdp: &str) -> bool {
    lines(sdp).any(|l| l.trim_end() == "a=ice-lite")
}

/// Connection addresses of all host candidates (in order, duplicates kept).
pub fn host_candidate_addresses(sdp: &str) -> Vec<String> {
    lines(sdp)
        .filter_map(parse_candidate)
        .filter(|c| c.typ == "host")
        .map(|c| c.address.to_string())
        .collect()
}

/// `"IPv6"` for an address containing a colon, `"IPv4"` otherwise.
pub fn address_family(address: &str) -> &'static str {
    if address.contains(':') {
        "IPv6"
    } else {
        "IPv4"
    }
}

/// `host:port`, with brackets around IPv6 addresses.
pub fn format_endpoint(address: &str, port: u16) -> String {
    if address.contains(':') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

/// Rewrites `typ host` candidates: IPv4 ones advertise `public_ipv4` and IPv6 ones
/// `public_ipv6`, when given. Candidates of a family without a replacement keep their address.
/// `c=` lines are pointed at the replacement of their family as well.
///
/// Candidates that become identical (same transport, address and port) after the rewrite are
/// collapsed into one, so a VM with several interfaces bound on the same port produces a
/// single candidate per family. Non-host candidates and all other lines are left untouched.
/// Line endings (`\r\n` or `\n`) are preserved.
pub fn rewrite_host_candidates(
    sdp: &str,
    public_ipv4: Option<&str>,
    public_ipv6: Option<&str>,
) -> String {
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len() + 64);
    let mut seen: Vec<String> = Vec::new();
    for line in lines(sdp) {
        if let Some(c) = parse_candidate(line) {
            if c.typ == "host" {
                let replacement = if c.address.contains(':') {
                    public_ipv6
                } else {
                    public_ipv4
                };
                let address = replacement.unwrap_or(c.address);
                let key = format!(
                    "{}|{}|{}",
                    c.transport.to_ascii_lowercase(),
                    address,
                    c.port
                );
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                out.push_str(&c.with_address(address));
                out.push_str(eol);
                continue;
            }
        } else if let Some(rest) = line.strip_prefix("c=IN IP4 ") {
            if let Some(ip) = public_ipv4 {
                if rest.trim_end() != "0.0.0.0" {
                    out.push_str("c=IN IP4 ");
                    out.push_str(ip);
                    out.push_str(eol);
                    continue;
                }
            }
        } else if let Some(rest) = line.strip_prefix("c=IN IP6 ") {
            if let Some(ip) = public_ipv6 {
                if rest.trim_end() != "::" {
                    out.push_str("c=IN IP6 ");
                    out.push_str(ip);
                    out.push_str(eol);
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push_str(eol);
    }
    out
}

fn lines(sdp: &str) -> impl Iterator<Item = &str> {
    sdp.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
}

/// Parsed `a=candidate:` line (RFC 8839 §5.1).
struct Candidate<'a> {
    foundation: &'a str,
    component: &'a str,
    transport: &'a str,
    priority: &'a str,
    address: &'a str,
    port: &'a str,
    typ: &'a str,
    /// Everything after `typ <type>` (raddr/rport/generation/... extensions), may be empty.
    rest: &'a str,
}

fn parse_candidate(line: &str) -> Option<Candidate<'_>> {
    let body = line.strip_prefix("a=candidate:")?;
    let mut it = body.split_ascii_whitespace();
    let foundation = it.next()?;
    let component = it.next()?;
    let transport = it.next()?;
    let priority = it.next()?;
    let address = it.next()?;
    let port = it.next()?;
    if it.next()? != "typ" {
        return None;
    }
    let typ = it.next()?;
    let rest = it.collect::<Vec<_>>().join(" ");
    // `rest` is owned; store it back as a slice of the original line to avoid allocation
    // in the common case.
    let rest = if rest.is_empty() {
        ""
    } else {
        let idx = line.rfind(&rest)?;
        &line[idx..]
    };
    Some(Candidate {
        foundation,
        component,
        transport,
        priority,
        address,
        port,
        typ,
        rest,
    })
}

impl Candidate<'_> {
    fn with_address(&self, address: &str) -> String {
        let mut s = format!(
            "a=candidate:{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            self.transport,
            self.priority,
            address,
            self.port,
            self.typ
        );
        if !self.rest.is_empty() {
            s.push(' ');
            s.push_str(self.rest);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "v=0\r\n\
o=- 3901 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=ice-lite\r\n\
a=group:BUNDLE 0 1\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
c=IN IP4 10.0.0.7\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=candidate:1234 1 udp 2130706431 10.0.0.7 50000 typ host generation 0\r\n\
a=candidate:5678 1 UDP 2130706175 172.17.0.1 50000 typ host generation 0\r\n\
a=candidate:abcd 1 udp 2130705919 2603:c020:4000:1::5 50000 typ host generation 0\r\n\
a=candidate:9999 1 udp 1694498815 203.0.113.9 50000 typ srflx raddr 10.0.0.7 rport 50000\r\n\
a=end-of-candidates\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:1\r\n";

    #[test]
    fn inspection() {
        assert!(has_media(ANSWER, "video"));
        assert!(has_media(ANSWER, "application"));
        assert!(!has_media(ANSWER, "audio"));
        assert_eq!(candidate_count(ANSWER), 4);
        assert!(is_ice_lite(ANSWER));
        assert!(!is_ice_lite("v=0\r\na=ice-options:trickle\r\n"));
        assert_eq!(
            host_candidate_addresses(ANSWER),
            vec![
                "10.0.0.7".to_string(),
                "172.17.0.1".to_string(),
                "2603:c020:4000:1::5".to_string()
            ]
        );
        assert_eq!(address_family("10.0.0.7"), "IPv4");
        assert_eq!(address_family("2603:c020:4000:1::5"), "IPv6");
        assert_eq!(format_endpoint("10.0.0.7", 50000), "10.0.0.7:50000");
        assert_eq!(format_endpoint("2603::5", 50001), "[2603::5]:50001");
    }

    #[test]
    fn rewrite_replaces_ipv4_hosts_and_keeps_ipv6() {
        let out = rewrite_host_candidates(ANSWER, Some("129.146.1.2"), None);
        assert!(out.contains(
            "a=candidate:1234 1 udp 2130706431 129.146.1.2 50000 typ host generation 0\r\n"
        ));
        // The second IPv4 host candidate (other interface, same port) collapses into the first.
        assert!(!out.contains("172.17.0.1"));
        assert!(!out.contains("10.0.0.7 50000"));
        // The IPv6 host candidate is global: kept verbatim.
        assert!(out.contains(
            "a=candidate:abcd 1 udp 2130705919 2603:c020:4000:1::5 50000 typ host generation 0\r\n"
        ));
        assert_eq!(candidate_count(&out), 3);
        // srflx candidate untouched.
        assert!(out.contains(
            "a=candidate:9999 1 udp 1694498815 203.0.113.9 50000 typ srflx raddr 10.0.0.7 rport 50000\r\n"
        ));
        // c= lines: real address rewritten, 0.0.0.0 placeholder kept.
        assert!(out.contains("c=IN IP4 129.146.1.2\r\n"));
        assert!(out.contains("c=IN IP4 0.0.0.0\r\n"));
        // Everything else identical, CRLF preserved.
        assert!(out.contains("a=ice-lite\r\n"));
        assert!(out.contains("a=end-of-candidates\r\n"));
        assert!(out.ends_with("a=mid:1\r\n"));
        assert_eq!(
            host_candidate_addresses(&out),
            vec!["129.146.1.2".to_string(), "2603:c020:4000:1::5".to_string()]
        );
    }

    #[test]
    fn rewrite_with_ipv6_override_and_without_ipv4() {
        let out = rewrite_host_candidates(ANSWER, None, Some("2001:db8::42"));
        // No IPv4 replacement: private IPv4 candidates stay (deduped by address, so both remain).
        assert!(out.contains("10.0.0.7 50000 typ host"));
        assert!(out.contains("172.17.0.1 50000 typ host"));
        assert!(out.contains("c=IN IP4 10.0.0.7\r\n"));
        // IPv6 replaced.
        assert!(out.contains(
            "a=candidate:abcd 1 udp 2130705919 2001:db8::42 50000 typ host generation 0\r\n"
        ));
        assert!(!out.contains("2603:c020"));
        assert_eq!(candidate_count(&out), 4);
        let both = rewrite_host_candidates(ANSWER, Some("129.146.1.2"), Some("2001:db8::42"));
        assert_eq!(
            host_candidate_addresses(&both),
            vec!["129.146.1.2".to_string(), "2001:db8::42".to_string()]
        );
    }

    #[test]
    fn rewrite_keeps_lf_endings_and_untouched_input() {
        let lf = "v=0\na=candidate:1 1 udp 1 192.168.1.5 50001 typ host\n";
        let out = rewrite_host_candidates(lf, Some("8.8.4.4"), None);
        assert_eq!(out, "v=0\na=candidate:1 1 udp 1 8.8.4.4 50001 typ host\n");
        let plain = "v=0\r\ns=-\r\n";
        assert_eq!(rewrite_host_candidates(plain, Some("1.2.3.4"), None), plain);
        // Nothing to replace: identical output.
        assert_eq!(rewrite_host_candidates(lf, None, None), lf);
        let v6 = "c=IN IP6 2001:db8::1\r\nc=IN IP6 ::\r\n";
        assert_eq!(
            rewrite_host_candidates(v6, None, Some("2001:db8::9")),
            "c=IN IP6 2001:db8::9\r\nc=IN IP6 ::\r\n"
        );
    }

    #[test]
    fn malformed_candidate_lines_pass_through() {
        let bad = "a=candidate:garbage\r\n";
        assert_eq!(rewrite_host_candidates(bad, Some("1.2.3.4"), None), bad);
        assert_eq!(candidate_count(bad), 1);
        assert!(host_candidate_addresses(bad).is_empty());
    }
}
