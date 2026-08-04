//! Ask-card projection and answering.
//!
//! Implements Wave-1 daemon deliverable 9 (`DESIGN.md` §4.1.1), §2.4, and
//! §3.4.1.
//!
//! # Permission answering is not a control frame
//!
//! §2.4 is explicit: it is `POST /message/{id}/ask`. The endpoint publishes a
//! **threaded kind:9 reply** carrying `askReplyContent` / `askReplyMentions`
//! semantics from `desktop/src/features/messages/lib/askCard.ts`, **including
//! the `broadcast` tag** — a thread-only reply never reaches the channel window
//! and the card's answered-state derivation breaks without it.
//!
//! This matters because the adjacent mechanism is a control frame, and
//! `POST /agent/{pk}/control` accepts exactly two payloads
//! ([`crate::observer::ControlPayload`]). Routing an answer there would be
//! logged-and-dropped by the harness — silently.

use serde::{Deserialize, Serialize};

/// Maximum options on a valid ask card.
///
/// Origin: `desktop/src/features/messages/lib/askCard.ts:31`
/// (`ASK_MAX_OPTIONS`). More than this parses to `null`, i.e. "not an ask
/// card", not "an ask card with too many options".
pub const ASK_MAX_OPTIONS: usize = 20;

/// Supported ask-card schema version. `v: 2` parses to `null` (§5.2).
pub const ASK_VERSION: u32 = 1;

/// Tag name that must ride on an ask answer.
///
/// Without it a thread-only reply never reaches the channel window and the
/// card's answered-state derivation breaks (§2.4).
pub const BROADCAST_TAG: &str = "broadcast";

/// A parsed ask card, projected onto the hydrated message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskCard {
    /// Schema version; must equal [`ASK_VERSION`].
    pub v: u32,
    /// The question text. Empty is invalid.
    pub question: String,
    /// At most [`ASK_MAX_OPTIONS`] options.
    pub options: Vec<AskOption>,
    /// Routing: [`AskRouting::AskOwner`] is actionable,
    /// [`AskRouting::Auto`] is informational and already answered with no
    /// affordance (§5.3's `agent-ask` fixture covers both).
    pub routing: AskRouting,
}

/// One selectable answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskOption {
    /// Machine value published in the answer.
    pub value: String,
    /// Human label rendered in the card.
    pub label: String,
}

/// How the ask was routed, which decides whether the card is actionable.
///
/// Mirrors the harness's `PermissionRouting` (`crates/buzz-acp/src/acp.rs:283`).
/// **The precondition is usually absent** (§3.4.1): the harness default is
/// `PermissionRouting::Auto`, i.e. permissions are auto-approved and never reach
/// the owner at all unless the agent was deployed with
/// `--permission-mode askOwner`.
///
/// The wire names are `snake_case` because they are daemon→TUI vocabulary, not
/// a relay contract — the harness's `askOwner` spelling is a *clap* alias on
/// `PermissionMode`, whose own wire string to the agent is `default`
/// (`crates/buzz-acp/src/config.rs:155`). Carrying that spelling here would
/// imply a wire compatibility that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskRouting {
    /// The owner must answer; the turn blocks meanwhile. Actionable — `⏎`
    /// publishes the threaded reply.
    AskOwner,
    /// Auto-approved with the request's own `allow_once` option, unattended.
    /// Renders **informational and already-answered**, with no affordance:
    /// "an actionable-looking control that cannot act is worse than no
    /// control" (§3.4.1). **No key answers it** (§5.5).
    Auto,
}

/// Ask-card parsing and answering.
///
/// TODO(wave1, §4.1.1 deliverable 9): parse `["ask", json]` off kind:9/40002
/// with `askCard.ts` validation **verbatim** — `v == 1`, [`ASK_MAX_OPTIONS`],
/// option shape — expose it on the hydrated message, emit `agent.ask.open` /
/// `agent.ask.answered`, derive the awaiting count for §3.4's fleet view, and
/// serve `POST /message/{id}/ask` as a threaded kind:9 reply carrying the
/// [`BROADCAST_TAG`].
///
/// §5.2's `ask card parse` row mirrors `askCard.test.mjs`: malformed json →
/// null; `v: 2` → null; `options: "nope"` → null; `> ASK_MAX_OPTIONS` → null.
#[derive(Debug, Default)]
pub struct AskCards {
    _private: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Origin cited in the module docs.
    #[test]
    fn limits_match_the_desktop() {
        assert_eq!(ASK_MAX_OPTIONS, 20);
        assert_eq!(ASK_VERSION, 1);
    }

    /// §2.4: the answer carries the broadcast tag, or the card never resolves.
    #[test]
    fn broadcast_tag_name_is_fixed() {
        assert_eq!(BROADCAST_TAG, "broadcast");
    }

    /// §3.4.1/§5.5: the two routings are distinct states with distinct wire
    /// names — collapsing them would render an actionable affordance on a card
    /// that cannot act.
    #[test]
    fn routings_are_distinct_and_wire_stable() {
        assert_ne!(AskRouting::AskOwner, AskRouting::Auto);
        assert_eq!(
            serde_json::to_string(&AskRouting::AskOwner).unwrap(),
            "\"ask_owner\""
        );
        assert_eq!(
            serde_json::to_string(&AskRouting::Auto).unwrap(),
            "\"auto\""
        );
    }
}
