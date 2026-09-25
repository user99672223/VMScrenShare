//! HTTP signalling types.
//!
//! The client sends `POST /offer` with a JSON [`SessionDescription`] (`{"type":"offer","sdp":...}`)
//! and receives the complete answer (all ICE candidates included, no trickle) in the same
//! format. The JSON layout is identical to the W3C `RTCSessionDescriptionInit` dictionary and
//! to the serde representation used by webrtc-rs, so either side can deserialize directly.

use serde::{Deserialize, Serialize};

/// Path of the signalling endpoint on the server (`127.0.0.1:8080`).
pub const OFFER_PATH: &str = "/offer";
/// Liveness endpoint, answers `200 OK` with the body `vmdesk`.
pub const HEALTH_PATH: &str = "/health";
/// Body returned by [`HEALTH_PATH`].
pub const HEALTH_BODY: &str = "vmdesk";

/// Default signalling URL used by the client (reached through `ssh -L 8080:127.0.0.1:8080`).
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8080";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDescription {
    /// `"offer"` or `"answer"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub sdp: String,
}

impl SessionDescription {
    pub fn offer(sdp: impl Into<String>) -> Self {
        Self {
            kind: "offer".into(),
            sdp: sdp.into(),
        }
    }

    pub fn answer(sdp: impl Into<String>) -> Self {
        Self {
            kind: "answer".into(),
            sdp: sdp.into(),
        }
    }

    pub fn is_offer(&self) -> bool {
        self.kind == "offer"
    }
}

/// Error body returned by the server for a rejected offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_layout_matches_w3c() {
        let d = SessionDescription::offer("v=0\r\n");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, r#"{"type":"offer","sdp":"v=0\r\n"}"#);
        let back: SessionDescription = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        assert!(back.is_offer());
        assert!(!SessionDescription::answer("x").is_offer());
    }
}
