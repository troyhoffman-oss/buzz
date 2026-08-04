//! Unread and read-state (NIP-RS).
//!
//! Implements Wave-1 daemon deliverable 5 (`DESIGN.md` §4.1.1): kind 30078
//! `d=read-state:<slot>`, NIP-44 self-encrypted `{v:1, client_id, contexts}`,
//! with context keys as bare `<channelUuid>` / `thread:<id>` / `msg:<id>`.
//!
//! §5.2 calls this "the highest-risk port", and §4.1.4 exit criterion 6 gates
//! the wave on the unread divider surviving a reconnect burst, a tier change, a
//! daemon restart, and a second client marking a different channel read.

use serde::{Deserialize, Serialize};

/// Maximum encrypted bytes per read-state slot (§4.1.1 deliverable 5).
pub const MAX_SLOT_BYTES: usize = 32 * 1024;

/// Maximum number of slots before the oldest contexts are pruned.
pub const MAX_SLOTS: usize = 8;

/// Maximum tracked contexts across all slots.
pub const MAX_CONTEXTS: usize = 10_000;

/// Horizon after which `msg:` and `thread:` contexts are pruned, in days.
pub const CONTEXT_HORIZON_DAYS: u32 = 7;

/// Debounce before publishing a read-state update, in milliseconds.
pub const PUBLISH_DEBOUNCE_MS: u64 = 5_000;

/// A read-state context key (§4.1.1 deliverable 5).
///
/// Keys are **bare**: a channel context is the raw uuid, not `channel:<uuid>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextKey {
    /// Any of the three forms, held as its wire string.
    Raw(String),
}

/// The read-state frontier.
///
/// TODO(wave1, §4.1.1 deliverable 5): implement the **hierarchical frontier**
/// `effective(ctx) = max(merged[ctx], effective(parent(ctx)))` with the
/// thread→channel parent link **derived from the event graph at evaluation
/// time, never serialized**; max-wins merge across devices; slot split at
/// [`MAX_SLOT_BYTES`]; the [`MAX_SLOTS`] cap; [`CONTEXT_HORIZON_DAYS`] pruning
/// for `msg:`/`thread:`; and `msg:` markers grow-only per reply id so reading
/// an ancestor never covers a descendant.
///
/// Unread counting is gated by `isConversationalUnreadKind` so system/job/huddle
/// rows never create phantom unreads — see [`crate::timeline::TIMELINE_KINDS`].
///
/// The desktop's `readStateManager.ts` rotates `slotId` when another
/// `client_id` squats its `d`-tag; that conflict detection is the evidence that
/// the two-daemons-on-one-identity hazard of §2.3 is real, and the daemon
/// inherits the same behaviour.
#[derive(Debug, Default)]
pub struct ReadState {
    _private: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §4.1.1 deliverable 5 fixes every one of these bounds.
    #[test]
    fn bounds_match_the_design() {
        assert_eq!(MAX_SLOT_BYTES, 32 * 1024);
        assert_eq!(MAX_SLOTS, 8);
        assert_eq!(MAX_CONTEXTS, 10_000);
        assert_eq!(CONTEXT_HORIZON_DAYS, 7);
        assert_eq!(PUBLISH_DEBOUNCE_MS, 5_000);
    }
}
