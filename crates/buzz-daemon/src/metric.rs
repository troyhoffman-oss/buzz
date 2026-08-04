//! NIP-AM turn metrics (kind 44200).
//!
//! Implements Wave-1 daemon deliverable 12 (`DESIGN.md` §4.1.1), §2.4, and
//! §3.4.1's usage pane.
//!
//! §1.2 calls this out as the flagship win: 44200 is "archived and never
//! displayed anywhere" on the desktop, so the TUI can be *better* here on day
//! one rather than chasing parity.
//!
//! # `#p = self` is mandatory, not optional
//!
//! §2.4: the `ids` exemption to the relay's p-gate has two carve-outs —
//! `RESULT_GATED_KINDS = [KIND_DM_VISIBILITY, KIND_AGENT_TURN_METRIC]` lose the
//! exemption when named explicitly. So `GET /agent/{pk}/metric` **must** carry
//! `#p = self`; an `{ids: […], kinds: [44200]}` lookup is refused. Specified in
//! the design because the endpoint would otherwise ship 403-ing.

use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};

/// The NIP-AM turn-metric kind.
pub const KIND_AGENT_TURN_METRIC: u32 = buzz_core::kind::KIND_AGENT_TURN_METRIC;

/// A decoded turn metric.
///
/// Every count is `Option`, and §5.2's `44200 decode` row is explicit about
/// why: **`null` ≠ 0**, `totalTokens` is never derived, a missing
/// context-window denominator renders `—` **and no bar**, cost is suppressed as
/// a single figure when more than one model appears in a session, an unknown
/// `stopReason` becomes `unknown` rather than being dropped, and unknown fields
/// are ignored rather than failing the decode.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnMetric {
    /// Input tokens.
    pub tokens_in: Option<u64>,
    /// Output tokens.
    pub tokens_out: Option<u64>,
    /// Cache-read tokens.
    pub cache_read: Option<u64>,
    /// Cache-write tokens.
    pub cache_write: Option<u64>,
    /// Cost in USD for this turn.
    pub cost_usd: Option<f64>,
    /// Model identifier.
    pub model: Option<String>,
    /// Context-window denominator. `None` → §3.4.1 renders `—` and no bar.
    pub context_window: Option<u64>,
    /// Why the turn stopped; an unrecognized value becomes `unknown`.
    pub stop_reason: Option<String>,
}

impl TurnMetric {
    /// Total tokens, **only** when both halves were reported.
    ///
    /// §5.2: `totalTokens` is never derived. Returning `None` when either half
    /// is missing is what keeps a partially-reported turn from rendering as a
    /// confident total.
    pub fn total_tokens(&self) -> Option<u64> {
        match (self.tokens_in, self.tokens_out) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        }
    }

    /// Context-usage fraction, or `None` when the model reported no
    /// denominator. §3.4.1: render `—` and **no bar**, never a fabricated 0%.
    pub fn context_fraction(&self) -> Option<f64> {
        match (self.total_tokens(), self.context_window) {
            (Some(used), Some(window)) if window > 0 => Some(used as f64 / window as f64),
            _ => None,
        }
    }
}

/// Build the `/agent/{pk}/metric` filter, which **must** carry `#p = self`.
///
/// [`DaemonError::KindlessFilter`] is impossible here by construction; the
/// self-pubkey requirement is asserted instead, because that is the failure
/// mode §2.4 warns would otherwise ship 403-ing.
pub fn build_metric_filter(agent_pubkey: &str, self_pubkey: &str) -> Result<serde_json::Value> {
    if self_pubkey.is_empty() {
        return Err(DaemonError::KindlessFilter {
            context: "agent metric requires #p = self",
        });
    }
    Ok(serde_json::json!({
        "kinds": [KIND_AGENT_TURN_METRIC],
        "#p": [self_pubkey],
        "authors": [agent_pubkey],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_buzz_core() {
        assert_eq!(KIND_AGENT_TURN_METRIC, 44200);
    }

    /// §2.4: `#p = self` is mandatory — the `ids` exemption does not apply to
    /// `RESULT_GATED_KINDS`.
    #[test]
    fn metric_filter_carries_p_self_and_explicit_kinds() {
        let filter = build_metric_filter(&"aa".repeat(32), &"bb".repeat(32)).unwrap();
        assert_eq!(filter["#p"], serde_json::json!(["bb".repeat(32)]));
        crate::search::assert_explicit_kinds(&filter, "metric").unwrap();
    }

    #[test]
    fn metric_filter_refuses_an_empty_self_pubkey() {
        assert!(build_metric_filter(&"aa".repeat(32), "").is_err());
    }

    /// §5.2: `null` ≠ 0 and `totalTokens` is never derived.
    #[test]
    fn total_tokens_needs_both_halves() {
        let partial = TurnMetric {
            tokens_in: Some(100),
            ..Default::default()
        };
        assert_eq!(partial.total_tokens(), None);
        let full = TurnMetric {
            tokens_in: Some(100),
            tokens_out: Some(20),
            ..Default::default()
        };
        assert_eq!(full.total_tokens(), Some(120));
    }

    /// §3.4.1: an absent context-window denominator renders `—` and no bar.
    #[test]
    fn absent_context_window_yields_no_fraction() {
        let m = TurnMetric {
            tokens_in: Some(58_204),
            tokens_out: Some(0),
            context_window: None,
            ..Default::default()
        };
        assert_eq!(m.context_fraction(), None);
    }

    #[test]
    fn zero_context_window_does_not_divide_by_zero() {
        let m = TurnMetric {
            tokens_in: Some(1),
            tokens_out: Some(1),
            context_window: Some(0),
            ..Default::default()
        };
        assert_eq!(m.context_fraction(), None);
    }
}
