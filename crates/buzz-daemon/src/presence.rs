//! Presence: two sources, three states.
//!
//! Implements Wave-1 daemon deliverable 11 (`DESIGN.md` §4.1.1) and §2.4.
//!
//! Kind 20001 is *ephemeral* — `is_ephemeral(20001)` is true and the relay never
//! stores it — so a daemon that starts after an agent's last beat sees nothing
//! until the next one. Durable last-seen comes from **kind 40902**
//! ([`buzz_core::kind::KIND_PRESENCE_SNAPSHOT`]), which is in the Wave-1 source
//! set.
//!
//! The rule: live presence from 20001; cold-start and last-seen from 40902; and
//! an explicit **`unknown`** state distinct from `offline` for the window before
//! the first beat. Rendering `offline` for "I just started and have not heard
//! anything yet" is exactly the looks-idle-while-the-socket-is-dead failure
//! §1.3 property 3 forbids.
//!
//! # Remote-agent liveness asymmetry
//!
//! §3.1 applies verbatim: a provider-backed agent's `deployed` status never
//! clears (the provider protocol has no undeploy), so `backend_agent_id` being
//! set says nothing about liveness. **Liveness for remote agents comes only from
//! relay presence.** The TUI renders presence, and shows deploy status only in
//! the agent detail pane, explicitly labelled "last deploy succeeded" rather
//! than "running".

use serde::{Deserialize, Serialize};

/// Ephemeral live-presence beat. Never stored by the relay.
pub const KIND_PRESENCE_BEAT: u32 = 20001;

/// Durable presence snapshot — the cold-start and last-seen source.
pub const KIND_PRESENCE_SNAPSHOT: u32 = buzz_core::kind::KIND_PRESENCE_SNAPSHOT;

/// Presence as rendered by §3.1's four-state glyph set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// `⬤` — a live 20001 beat within the window.
    Present,
    /// `◐` — waking.
    Waking,
    /// `○` — known offline from a 40902 snapshot.
    Offline,
    /// `◌` — **no beat seen since this daemon started and no 40902 snapshot
    /// yet.** Never collapsed into [`Presence::Offline`].
    Unknown,
}

/// Presence tracking.
///
/// TODO(wave1, §4.1.1 deliverable 11): subscribe 20001 for live beats, load
/// 40902 for cold-start and last-seen, and hold [`Presence::Unknown`] until one
/// of the two has spoken.
#[derive(Debug, Default)]
pub struct PresenceTracker {
    _private: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_match_the_design() {
        assert_eq!(KIND_PRESENCE_BEAT, 20001);
        assert_eq!(KIND_PRESENCE_SNAPSHOT, 40902);
    }

    /// The 20000–29999 range is ephemeral, which is *why* 40902 exists as the
    /// durable half.
    #[test]
    fn the_beat_kind_is_in_the_ephemeral_range() {
        assert!((20_000..30_000).contains(&KIND_PRESENCE_BEAT));
        assert!(!(20_000..30_000).contains(&KIND_PRESENCE_SNAPSHOT));
    }

    /// §2.4/§3.1: `unknown` is a distinct state, not a synonym for offline.
    #[test]
    fn unknown_is_distinct_from_offline() {
        assert_ne!(Presence::Unknown, Presence::Offline);
        assert_eq!(
            serde_json::to_string(&Presence::Unknown).unwrap(),
            "\"unknown\""
        );
    }
}
