//! Full-text search over NIP-50, and the global kinds invariant.
//!
//! Implements Wave-1 daemon deliverable 7 (`DESIGN.md` §4.1.1) and §3.5.
//!
//! # The kinds invariant is global, not search-specific
//!
//! §2.4: **no filter leaves the daemon without an explicit `kinds`.** §3.5
//! previously stated this as a search rule, which would mislead an implementer
//! into setting `kinds` on `/search` and forgetting it on `/message/{id}`. The
//! actual gate is `p_gated_filters_authorized`
//! (`crates/buzz-relay/src/handlers/req.rs` → `crates/buzz-core/src/kind.rs`):
//! a filter that *can match* any `P_GATED_KIND` is refused unless its `#p`
//! values equal the authenticated reader's pubkey — and a filter with **no**
//! `kinds` can match everything. It applies to `REQ` and `/query` equally,
//! closing as `restricted:` or `403` depending on transport.
//!
//! The `ids` exemption has two carve-outs that bite a Wave-1 endpoint:
//! `RESULT_GATED_KINDS` (`KIND_DM_VISIBILITY`, `KIND_AGENT_TURN_METRIC`) lose
//! the exemption when named explicitly, so `/agent/{pk}/metric` **must** carry
//! `#p = self` — see [`crate::metric`].

use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};

/// Default search kinds (§3.5).
///
/// An open-ended search would hit the relay's p-gate and return 403, so this is
/// never empty.
pub const DEFAULT_SEARCH_KINDS: [u32; 4] = [9, 40002, 45001, 45003];

/// Search-as-you-type debounce, in milliseconds. Protects the relay's FTS, not
/// just the render loop (§3.5).
pub const SEARCH_DEBOUNCE_MS: u64 = 150;

/// Parsed Slack-style search operators (§3.5).
///
/// TODO(wave1, §4.1.1 deliverable 7): parse identically to the desktop's
/// `parseSearchOperators.ts`. Operators must start at a **token boundary** —
/// deliberately not `\b`, so `built-in:react` and `https://x.com/in:foo` are not
/// misparsed. `after:` is local start-of-day inclusive; `before:` is one second
/// before local start-of-day, because NIP-01 `until` is inclusive and Slack
/// excludes the named day. An invalid operator value stays in the FTS text
/// rather than erroring.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Residual free text after operators are lifted out.
    pub text: String,
    /// `from:` — may be a display name, which is what makes
    /// [`AmbiguousAuthor`] necessary.
    pub from: Option<String>,
    /// `in:` — channel scope.
    pub in_channel: Option<String>,
    /// `after:` as local start-of-day, inclusive.
    pub after: Option<i64>,
    /// `before:` as local start-of-day minus one second.
    pub before: Option<i64>,
}

/// The `409 ambiguous_author` body of §3.5.
///
/// Rendered as a disambiguation list — **never a silent mix of authors**.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousAuthor {
    /// The name that matched more than one identity.
    pub query: String,
    /// Every candidate, so the client can disambiguate without a second round
    /// trip.
    pub candidates: Vec<String>,
}

/// Assert a filter carries explicit `kinds` before it leaves the daemon.
///
/// The global invariant of §2.4, enforced here so no endpoint can forget it.
pub fn assert_explicit_kinds(filter: &serde_json::Value, context: &'static str) -> Result<()> {
    let kinds = filter.get("kinds").and_then(|k| k.as_array());
    match kinds {
        Some(k) if !k.is_empty() => Ok(()),
        _ => Err(DaemonError::KindlessFilter { context }),
    }
}

/// Build a search filter with `kinds` always set.
pub fn build_search_filter(query: &str, kinds: Option<&[u32]>) -> serde_json::Value {
    let kinds = kinds.unwrap_or(&DEFAULT_SEARCH_KINDS);
    serde_json::json!({
        "search": query,
        "kinds": kinds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §3.5 fixes the default kind set.
    #[test]
    fn default_kinds_match_the_design() {
        assert_eq!(DEFAULT_SEARCH_KINDS, [9, 40002, 45001, 45003]);
    }

    /// §2.4: a kindless filter is refused before it reaches the relay's p-gate.
    #[test]
    fn kindless_filters_are_refused() {
        let err = assert_explicit_kinds(&serde_json::json!({"search": "x"}), "search").unwrap_err();
        assert_eq!(err.code(), "kindless_filter");
    }

    /// An empty `kinds` array can match everything just as a missing one can.
    #[test]
    fn empty_kinds_array_is_also_refused() {
        let filter = serde_json::json!({"kinds": [], "search": "x"});
        assert!(assert_explicit_kinds(&filter, "search").is_err());
    }

    #[test]
    fn a_built_search_filter_satisfies_the_invariant() {
        let filter = build_search_filter("read-state slots", None);
        assert_explicit_kinds(&filter, "search").unwrap();
        assert_eq!(filter["kinds"], serde_json::json!(DEFAULT_SEARCH_KINDS));
    }

    #[test]
    fn explicit_kinds_override_the_default() {
        let filter = build_search_filter("q", Some(&[9]));
        assert_eq!(filter["kinds"], serde_json::json!([9]));
    }

    #[test]
    fn debounce_matches_the_design() {
        assert_eq!(SEARCH_DEBOUNCE_MS, 150);
    }
}
