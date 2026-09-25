//! Minimal SDP inspection and rewriting.
//!
//! The server is ICE-lite behind a 1:1 NAT (OCI assigns the public IP outside the VM), so the
//! host candidates it gathers carry the VM's private address. Instead of relying on the WebRTC
//! stack to substitute the public address, the signalling server rewrites the answer with
//! [`rewrite_host_candidates`] before returning it. These helpers work on the SDP text only.

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

/// Rewrites every `typ host` candidate to advertise `public_ip` instead of the interface
/// address the socket is bound to, and points `c=` lines at it too.
///
/// Candidates that become identical (same transport, address and port) after the rewrite are
/// collapsed into one, so a VM with several interfaces bound on the same port produces a
/// single candidate. Non-host candidates and all other lines are left untouched. Line endings
/// (`\r\n` or `\n`) are preserved.
pub fn rewrite_host_candidates(sdp: &str, public_ip: &str) -> String {
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len() + 64);
    let mut seen: Vec<String> = Vec::new();
    for line in lines(sdp) {
        if let Some(c) = parse_candidate(line) {
            if c.typ == "host" {
                let key = format!(
                    "{}|{}|{}",
                    c.transport.to_ascii_lowercase(),
                    public_ip,
                    c.port
                );
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                out.push_str(&c.with_address(public_ip));
                out.push_str(eol);
                continue;
            }
        } else if let Some(rest) = line.strip_prefix("c=IN IP4 ") {
            let addr = rest.trim_end();
            if addr != "0.0.0.0" {
                out.push_str("c=IN IP4 ");
                out.push_str(public_ip);
                out.push_str(eol);
                continue;
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
        assert_eq!(candidate_count(ANSWER), 3);
        assert!(is_ice_lite(ANSWER));
        assert!(!is_ice_lite("v=0\r\na=ice-options:trickle\r\n"));
        assert_eq!(
            host_candidate_addresses(ANSWER),
            vec!["10.0.0.7".to_string(), "172.17.0.1".to_string()]
        );
    }

    #[test]
    fn rewrite_replaces_host_addresses_and_dedupes() {
        let out = rewrite_host_candidates(ANSWER, "129.146.1.2");
        assert!(out.contains(
            "a=candidate:1234 1 udp 2130706431 129.146.1.2 50000 typ host generation 0\r\n"
        ));
        // The second host candidate (other interface, same port) collapses into the first.
        assert!(!out.contains("172.17.0.1"));
        assert!(!out.contains("10.0.0.7 50000"));
        assert_eq!(candidate_count(&out), 2);
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
            vec!["129.146.1.2".to_string()]
        );
    }

    #[test]
    fn rewrite_keeps_lf_endings_and_untouched_input() {
        let lf = "v=0\na=candidate:1 1 udp 1 192.168.1.5 50001 typ host\n";
        let out = rewrite_host_candidates(lf, "8.8.4.4");
        assert_eq!(out, "v=0\na=candidate:1 1 udp 1 8.8.4.4 50001 typ host\n");
        let plain = "v=0\r\ns=-\r\n";
        assert_eq!(rewrite_host_candidates(plain, "1.2.3.4"), plain);
    }

    #[test]
    fn malformed_candidate_lines_pass_through() {
        let bad = "a=candidate:garbage\r\n";
        assert_eq!(rewrite_host_candidates(bad, "1.2.3.4"), bad);
        assert_eq!(candidate_count(bad), 1);
        assert!(host_candidate_addresses(bad).is_empty());
    }
}
