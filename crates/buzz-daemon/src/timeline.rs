//! Timeline fetch — the NIP-CW server-assembled window.
//!
//! Implements `DESIGN.md` §2.4 [D-10] and Wave-1 daemon deliverable 4 (§4.1.1).
//!
//! An earlier draft specified a two-query fetch (content kinds by time window,
//! aux kinds by `#e`) and called it "verbatim". It is not verbatim — it is the
//! *pre*-NIP-CW desktop path, and reimplementing it reintroduces exactly the
//! bugs NIP-CW exists to remove. The current desktop sends `top_level: true`,
//! `include_summaries: true`, `include_aux: true`, plus `(until, before_id)`
//! (`desktop/src-tauri/src/commands/channel_window.rs`). Three things the
//! two-query plan loses:
//!
//! - A plain `kinds` + `#h` filter **cannot express "not a reply"**, so `limit`
//!   counts raw events: a page of 50 may contain 3 top-level rows or 50.
//! - `39006` carries the authoritative `has_more`. NIP-CW §Client Behavior is
//!   explicit — *"A client MUST NOT stop paging on row count"*; an
//!   exact-multiple final page returns `limit` rows with `has_more: false`. The
//!   obvious implementation of the `{next, has_more}` contract (short page =
//!   done) is the precise bug the NIP forbids.
//! - Thread summaries (`39005`) arrive as relay-signed overlays with the
//!   window, and are the cheap source for §3.1's `⤷ 4` reply count.
//!
//! # The degradation branch is also Wave 1
//!
//! No valid `39006` (extension-unaware relay, or a strict parser rejecting the
//! filter) → reissue a clean standard filter with the extension keys removed
//! and assemble threads client-side, which is what `auxBackfill.ts` still does.
//! **Downgrade is a decision, not a fallback that happens by accident** — and a
//! downgrade that has never run is a downgrade that does not work, which is why
//! it ships in the same wave.

use serde::{Deserialize, Serialize};

use crate::cursor::Cursor;
use crate::error::{DaemonError, Result};

/// The timeline kind set, **verbatim** from
/// `desktop/src-tauri/src/commands/channel_window.rs`'s `TIMELINE_KINDS`.
///
/// §3.1: non-conversational kinds (40099 system, 43001–43006 job, 48100 huddle
/// started) render as their own dimmed rows and are excluded from the unread
/// pill, per `isConversationalUnreadKind`. Note **48100 only** — 48101–48103 are
/// neither fetched nor rendered in Wave 1. An earlier draft rendered them while
/// fetching only 48100, which is unimplementable as written and made a §5.2
/// test case assert on a path that cannot occur. They arrive with the read-only
/// huddle surface in Wave 4.
pub const TIMELINE_KINDS: [u32; 11] = [
    9,     // channel message
    40002, // rich message
    40008, // diff message — §3.1 gives these their own row
    40099, // system
    43001,
    43002,
    43003,
    43004,
    43005,
    43006,                                // job
    buzz_core::kind::KIND_HUDDLE_STARTED, // 48100 — and only 48100
];

/// Relay-signed thread summary overlay. Source of §3.1's `⤷ 4` reply count.
pub const KIND_THREAD_SUMMARY: u32 = buzz_core::kind::KIND_THREAD_SUMMARY;

/// Relay-signed window-bounds overlay. **The sole exhaustion authority** —
/// see [`WindowBounds::has_more`].
pub const KIND_WINDOW_BOUNDS: u32 = buzz_core::kind::KIND_WINDOW_BOUNDS;

/// The `kind:39006` overlay that terminates (or continues) paging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds {
    /// Authoritative exhaustion signal. **Row count is never used** —
    /// NIP-CW §Client Behavior: *"A client MUST NOT stop paging on row count."*
    pub has_more: bool,
    /// Cursor for the next page; `None` exactly when `has_more` is false.
    pub next_cursor: Option<String>,
    /// The `d`-tag binding echoed back from the request, used by
    /// [`check_bounds_integrity`].
    pub binding: String,
}

/// Build the NIP-CW window filter (§4.1.1 deliverable 4).
///
/// Mirrors `build_channel_window_filter` in the desktop, including the
/// composite `(until, before_id)` echoed from the previous page's `39006`.
///
/// `kinds` is always set — not as a search rule but as the **global daemon
/// invariant** of §2.4: the relay's `p_gated_filters_authorized` gate refuses a
/// filter that can match a `P_GATED_KIND`, and a filter with no `kinds` can
/// match everything.
pub fn build_window_filter(
    channel_id: &str,
    limit: u32,
    cursor: Option<&Cursor>,
) -> serde_json::Value {
    let mut filter = serde_json::Map::new();
    filter.insert("#h".into(), serde_json::json!([channel_id]));
    filter.insert("kinds".into(), serde_json::json!(TIMELINE_KINDS));
    filter.insert("limit".into(), serde_json::json!(limit));
    filter.insert("top_level".into(), serde_json::json!(true));
    filter.insert("include_summaries".into(), serde_json::json!(true));
    filter.insert("include_aux".into(), serde_json::json!(true));
    if let Some(cursor) = cursor {
        filter.insert("until".into(), serde_json::json!(cursor.until));
        filter.insert("before_id".into(), serde_json::json!(cursor.before_id));
    }
    serde_json::Value::Object(filter)
}

/// The degraded filter used when a relay serves no valid `39006` ([D-10]).
///
/// A clean *standard* filter with the extension keys removed. Threads are then
/// assembled client-side and aux is fetched by `#e` over loaded ids, which is
/// what `auxBackfill.ts` still does.
pub fn build_downgraded_filter(
    channel_id: &str,
    limit: u32,
    cursor: Option<&Cursor>,
) -> serde_json::Value {
    let mut filter = serde_json::Map::new();
    filter.insert("#h".into(), serde_json::json!([channel_id]));
    filter.insert("kinds".into(), serde_json::json!(TIMELINE_KINDS));
    filter.insert("limit".into(), serde_json::json!(limit));
    if let Some(cursor) = cursor {
        filter.insert("until".into(), serde_json::json!(cursor.until));
    }
    serde_json::Value::Object(filter)
}

/// Bounds-integrity checks per NIP-CW §Client Behavior step 5 ([D-10]).
///
/// Exactly one `39006`; its `d`-tag binding echoes the request cursor; content
/// parses; and `has_more = true ⇔ next_cursor ≠ null`. Anything else means
/// **discard the page and retry, never guess**.
pub fn check_bounds_integrity(bounds: &[WindowBounds], expected_binding: &str) -> Result<()> {
    match bounds.len() {
        1 => {}
        n => {
            return Err(DaemonError::WindowIntegrity(format!(
                "expected exactly one kind:39006 overlay, got {n}"
            )))
        }
    }
    let bounds = &bounds[0];
    if bounds.binding != expected_binding {
        return Err(DaemonError::WindowIntegrity(format!(
            "39006 binding {:?} does not echo the request cursor {:?}",
            bounds.binding, expected_binding
        )));
    }
    if bounds.has_more != bounds.next_cursor.is_some() {
        return Err(DaemonError::WindowIntegrity(
            "has_more must hold exactly when next_cursor is present".into(),
        ));
    }
    Ok(())
}

/// Whether paging continues. **The only** exhaustion authority ([D-10]).
///
/// Deliberately takes no row count: the signature is what makes "short page =
/// done" — the precise bug NIP-CW forbids — unrepresentable rather than merely
/// discouraged.
pub fn should_continue_paging(bounds: &WindowBounds) -> bool {
    bounds.has_more
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §3.1: `TIMELINE_KINDS` verbatim — **48100 only**.
    #[test]
    fn timeline_kinds_match_the_desktop_verbatim() {
        assert_eq!(
            TIMELINE_KINDS,
            [9, 40002, 40008, 40099, 43001, 43002, 43003, 43004, 43005, 43006, 48100]
        );
        for absent in [48101u32, 48102, 48103] {
            assert!(
                !TIMELINE_KINDS.contains(&absent),
                "{absent} is Wave 4, not Wave 1"
            );
        }
    }

    /// §2.4's global invariant: no filter leaves the daemon without `kinds`.
    #[test]
    fn both_filters_always_carry_explicit_kinds() {
        for filter in [
            build_window_filter("chan", 50, None),
            build_downgraded_filter("chan", 50, None),
        ] {
            assert!(filter.get("kinds").is_some(), "{filter}");
        }
    }

    /// [D-10]: the window filter carries the three extension keys; the
    /// downgraded one carries none of them.
    #[test]
    fn window_filter_carries_the_extension_keys_and_downgrade_does_not() {
        let window = build_window_filter("chan", 50, None);
        for key in ["top_level", "include_summaries", "include_aux"] {
            assert_eq!(window.get(key), Some(&serde_json::json!(true)), "{key}");
        }
        let downgraded = build_downgraded_filter("chan", 50, None);
        for key in ["top_level", "include_summaries", "include_aux"] {
            assert!(downgraded.get(key).is_none(), "{key} leaked into downgrade");
        }
    }

    /// [D-10]: the composite cursor is echoed as `(until, before_id)`.
    #[test]
    fn cursor_is_echoed_as_a_composite() {
        let cursor = Cursor {
            until: 1_700_000_000,
            before_id: "cd".repeat(32),
        };
        let filter = build_window_filter("chan", 50, Some(&cursor));
        assert_eq!(filter["until"], serde_json::json!(cursor.until));
        assert_eq!(filter["before_id"], serde_json::json!(cursor.before_id));
    }

    /// §5.2: "a **full** page with `has_more: false` terminates; row count is
    /// never used as an exhaustion signal."
    #[test]
    fn a_full_page_with_has_more_false_terminates() {
        let bounds = WindowBounds {
            has_more: false,
            next_cursor: None,
            binding: "b".into(),
        };
        assert!(!should_continue_paging(&bounds));
    }

    #[test]
    fn a_page_with_has_more_true_continues() {
        let bounds = WindowBounds {
            has_more: true,
            next_cursor: Some("c1.abc".into()),
            binding: "b".into(),
        };
        assert!(should_continue_paging(&bounds));
    }

    /// [D-10]: exactly one `39006`. Missing or duplicated discards the page.
    #[test]
    fn missing_or_duplicated_bounds_discards_the_page() {
        assert_eq!(
            check_bounds_integrity(&[], "b").unwrap_err().code(),
            "window_integrity"
        );
        let one = WindowBounds {
            has_more: false,
            next_cursor: None,
            binding: "b".into(),
        };
        assert!(check_bounds_integrity(&[one.clone(), one], "b").is_err());
    }

    /// [D-10]: the `d`-tag binding must echo the request cursor.
    #[test]
    fn mis_bound_overlay_discards_the_page() {
        let bounds = WindowBounds {
            has_more: false,
            next_cursor: None,
            binding: "someone-elses-page".into(),
        };
        assert!(check_bounds_integrity(&[bounds], "mine").is_err());
    }

    /// [D-10]: `has_more = true ⇔ next_cursor ≠ null`; a violation discards.
    #[test]
    fn has_more_must_agree_with_next_cursor() {
        let claims_more_without_cursor = WindowBounds {
            has_more: true,
            next_cursor: None,
            binding: "b".into(),
        };
        assert!(check_bounds_integrity(&[claims_more_without_cursor], "b").is_err());

        let cursor_without_more = WindowBounds {
            has_more: false,
            next_cursor: Some("c1.abc".into()),
            binding: "b".into(),
        };
        assert!(check_bounds_integrity(&[cursor_without_more], "b").is_err());
    }

    #[test]
    fn a_well_formed_page_passes() {
        let bounds = WindowBounds {
            has_more: true,
            next_cursor: Some("c1.abc".into()),
            binding: "b".into(),
        };
        check_bounds_integrity(&[bounds], "b").unwrap();
    }
}
