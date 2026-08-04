//! Mention candidates and the mention inbox.
//!
//! Implements Wave-1 daemon deliverable 6 (`DESIGN.md` §4.1.1) and the daemon
//! half of §2.4 [D-2].
//!
//! # [D-2] The TUI sends resolved pubkeys, never names
//!
//! `daemon-api.md` §3.3 allows `POST /channel/{id}/message` to omit `mentions`
//! and have the daemon run `extract_at_mentions_with_known` server-side. That
//! path stays for `curl` and for second clients, but the TUI **must not use
//! it**. `GET /mention/candidates` returns candidates that already carry their
//! pubkey; the composer's parts array holds the pubkey; the send carries an
//! explicit `mentions: [pubkey…]`.
//!
//! Two payoffs: "what you picked is what gets tagged" becomes true by
//! construction rather than by two implementations agreeing, and **frecency
//! ranking can live in the TUI** — where it belongs, since it is per-front-end
//! UI personalization, not protocol — with no risk of the ranked pick and the
//! resolved tag diverging. (Contrast drafts, which are daemon-held precisely
//! because they are per-*identity*; see §4.1.2.)

use serde::{Deserialize, Serialize};

/// Hard upper bound on `p` tags per message.
///
/// Origin: `crates/buzz-sdk/src/mentions.rs:38` (`MENTION_CAP`). This is a
/// **build-time** rejection in the SDK (`SdkError::TooManyMentions`), so it must
/// be surfaced *before* the send, not after: the daemon returns
/// `400 too_many_mentions {cap: 50, requested: n}` and the composer shows a
/// live `n of 50` counter at pick time. Failing at Enter on a message the
/// operator has already written is the worst possible place to learn about a
/// cap (§2.4).
pub const MENTION_CAP: usize = 50;

/// A mention candidate, carrying its **resolved pubkey** per [D-2].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MentionCandidate {
    /// Lowercase-hex pubkey. Present on every candidate — this is what makes
    /// "what you picked is what gets tagged" true by construction.
    pub pubkey: String,
    /// Display name shown in the picker.
    pub display_name: String,
    /// Whether this candidate came from the channel roster (which outranks the
    /// directory — §5.2's `candidate ranking` row).
    pub in_roster: bool,
    /// Whether this candidate is an agent rather than a human.
    pub is_agent: bool,
}

/// The `400 too_many_mentions` body of §2.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TooManyMentions {
    /// Always [`MENTION_CAP`].
    pub cap: usize,
    /// How many the client asked for.
    pub requested: usize,
}

/// Reject a mention list that would fail the SDK's build-time cap, *before* the
/// send.
pub fn check_mention_cap(requested: usize) -> Result<(), TooManyMentions> {
    if requested > MENTION_CAP {
        return Err(TooManyMentions {
            cap: MENTION_CAP,
            requested,
        });
    }
    Ok(())
}

/// Mention candidate resolution over the roster cache.
///
/// TODO(wave1, §4.1.1 deliverable 6): serve `GET /mention/candidates` and
/// `GET /mention/inbox` from the roster cache.
///
/// **`@channel` is deferred out of Wave 1** (§2.4). There is no wire
/// representation for it anywhere: no handling in `buzz-sdk`, none in the
/// relay, none in `desktop/src`. Client-side expansion to N `p` tags works at
/// 27 members and hard-fails at 51, which makes it a feature that breaks as a
/// community grows. It returns when it is specified as protocol — a marker tag
/// the relay expands, with fan-out accounted at the relay — not before. Tracked
/// as Q8 (§7.2).
#[derive(Debug, Default)]
pub struct Mentions {
    _private: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.4 cites `crates/buzz-sdk/src/mentions.rs:38`.
    #[test]
    fn cap_matches_the_sdk() {
        assert_eq!(MENTION_CAP, 50);
    }

    /// §2.4: the cap is surfaced before the send, not after.
    #[test]
    fn cap_is_inclusive_and_reports_both_numbers() {
        check_mention_cap(MENTION_CAP).unwrap();
        let err = check_mention_cap(MENTION_CAP + 1).unwrap_err();
        assert_eq!(err.cap, 50);
        assert_eq!(err.requested, 51);
    }

    /// [D-2]: every candidate carries a pubkey, so the picked identity and the
    /// tagged identity cannot diverge.
    #[test]
    fn candidates_carry_a_resolved_pubkey() {
        let json = serde_json::to_string(&MentionCandidate {
            pubkey: "aa".repeat(32),
            display_name: "matt".into(),
            in_roster: true,
            is_agent: false,
        })
        .unwrap();
        assert!(json.contains("pubkey"), "{json}");
    }
}
