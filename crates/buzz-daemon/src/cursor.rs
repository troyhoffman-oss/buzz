//! Pagination cursors: versioned, decodable, and not parsed by the client.
//!
//! Implements `DESIGN.md` §2.4 [D-6]. The wire form is
//! `c1.<base64url({"until":…,"before_id":…})>`.
//!
//! Two properties, each deliberate:
//! - The `c1.` prefix makes a future cursor format a **clean rejection** rather
//!   than a mis-parse.
//! - base64url-of-JSON makes a support conversation a `base64 -d` away.
//!
//! The TUI treats a cursor as opaque, and §6.4's `just tui-check-boundary`
//! asserts the front end never decodes one — the decodability is for humans
//! debugging, not for the client.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};

/// Version prefix of the current cursor format.
pub const CURSOR_PREFIX: &str = "c1.";

/// The composite `(until, before_id)` position of a NIP-CW window page ([D-10]).
///
/// Composite because `until` alone is ambiguous at second granularity: two
/// events sharing a `created_at` would be either skipped or repeated across a
/// page boundary depending on which side of the comparison they land. §5.2's
/// `composite cursor` row asserts exactly that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Newest `created_at` to include, unix seconds (NIP-01 `until` is
    /// inclusive).
    pub until: u64,
    /// Event id that ties-break within `until`.
    pub before_id: String,
}

impl Cursor {
    /// Encode to the `c1.<base64url(json)>` wire form.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("cursor is always serializable");
        format!(
            "{CURSOR_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    /// Decode from the wire form.
    ///
    /// A missing or unknown prefix is [`DaemonError::BadCursor`] — a clean
    /// rejection, which is the whole point of versioning the format.
    pub fn decode(raw: &str) -> Result<Self> {
        let body = raw
            .strip_prefix(CURSOR_PREFIX)
            .ok_or(DaemonError::BadCursor)?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| DaemonError::BadCursor)?;
        serde_json::from_slice(&bytes).map_err(|_| DaemonError::BadCursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor() -> Cursor {
        Cursor {
            until: 1_700_000_000,
            before_id: "ab".repeat(32),
        }
    }

    #[test]
    fn round_trips() {
        let encoded = cursor().encode();
        assert!(encoded.starts_with(CURSOR_PREFIX), "{encoded}");
        assert_eq!(Cursor::decode(&encoded).unwrap(), cursor());
    }

    /// [D-6]: "base64url-of-JSON makes a support conversation a `base64 -d`
    /// away."
    #[test]
    fn body_is_human_decodable_json() {
        let encoded = cursor().encode();
        let body = encoded.strip_prefix(CURSOR_PREFIX).unwrap();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"until\""), "{text}");
        assert!(text.contains("\"before_id\""), "{text}");
    }

    /// [D-6]: "the `c1.` prefix makes a future cursor format a clean rejection
    /// rather than a mis-parse."
    #[test]
    fn a_future_format_is_rejected_not_misparsed() {
        let future = format!(
            "c2.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"until\":1}")
        );
        assert_eq!(Cursor::decode(&future).unwrap_err().code(), "bad_cursor");
    }

    #[test]
    fn a_bare_or_corrupt_cursor_is_rejected() {
        assert!(Cursor::decode("1700000000").is_err());
        assert!(Cursor::decode("c1.!!!not-base64!!!").is_err());
        assert!(Cursor::decode("c1.").is_err());
    }
}
