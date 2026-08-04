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

/// How long a 20001 beat keeps someone `present` before they lapse.
///
/// The harness beats well inside this, so a lapse means beats actually stopped.
/// A window much shorter would flap on ordinary network jitter; much longer and
/// a dead agent reads as live for minutes, which is the §1.3 property 3 failure
/// this whole module exists to avoid.
pub const PRESENCE_BEAT_TTL_SECS: i64 = 90;

/// What is known about one pubkey's presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceRecord {
    /// The rendered state.
    pub state: Presence,
    /// Unix seconds of the newest evidence, from either source. `None` when
    /// nothing has ever been heard.
    pub last_seen: Option<i64>,
    /// Which source produced [`Self::state`], so a support conversation can
    /// tell "a live beat says present" from "a snapshot said present an hour
    /// ago".
    pub source: PresenceSource,
}

/// Where a presence record came from (§2.4: "presence has two sources").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceSource {
    /// A live ephemeral 20001 beat.
    Beat,
    /// A durable 40902 snapshot — cold start and last-seen.
    Snapshot,
    /// Nothing has been heard from either source.
    None,
}

impl PresenceRecord {
    /// The state of a pubkey nothing is known about.
    ///
    /// **`Unknown`, not `Offline`.** Rendering `offline` for "I just started and
    /// have not heard anything yet" is exactly the looks-idle-while-the-socket-
    /// is-dead failure §1.3 property 3 forbids.
    pub fn unknown() -> Self {
        Self {
            state: Presence::Unknown,
            last_seen: None,
            source: PresenceSource::None,
        }
    }
}

/// Build the live-presence subscription filter (20001).
pub fn build_beat_filter(pubkeys: &[String]) -> serde_json::Value {
    serde_json::json!({
        "kinds": [KIND_PRESENCE_BEAT],
        "authors": pubkeys,
    })
}

/// Build the durable-snapshot filter (40902) for cold start.
///
/// 20001 is *ephemeral* — `is_ephemeral(20001)` is true and the relay never
/// stores it — so a daemon that starts after an agent's last beat sees nothing
/// until the next one. This filter is what stops a fresh daemon from showing an
/// entire fleet as `unknown` for a full beat interval.
pub fn build_snapshot_filter(pubkeys: &[String]) -> serde_json::Value {
    serde_json::json!({
        "kinds": [KIND_PRESENCE_SNAPSHOT],
        "authors": pubkeys,
    })
}

/// Presence tracking: live from 20001, durable from 40902, `unknown` distinct
/// from `offline` (§4.1.1 deliverable 11, §2.4).
#[derive(Debug, Default)]
pub struct PresenceTracker {
    records: std::collections::BTreeMap<String, PresenceRecord>,
}

impl PresenceTracker {
    /// A tracker that knows nothing about anyone.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a live 20001 beat.
    ///
    /// Returns `true` when the rendered state changed, so the caller emits a
    /// `presence.update` frame only for a real transition. Presence is a
    /// coalescing topic ([D-5]) precisely because most beats change nothing.
    pub fn observe_beat(&mut self, pubkey: &str, status: &str, created_at: i64) -> bool {
        self.apply(
            pubkey,
            parse_status(status),
            created_at,
            PresenceSource::Beat,
        )
    }

    /// Apply a durable 40902 snapshot.
    ///
    /// A snapshot **never overrides newer beat evidence**: it is a record of
    /// what was true at its own `created_at`, and applying a stale one over a
    /// live beat would flip a working agent to offline.
    pub fn observe_snapshot(&mut self, pubkey: &str, status: &str, created_at: i64) -> bool {
        if let Some(existing) = self.records.get(pubkey) {
            if existing.last_seen.is_some_and(|seen| seen >= created_at) {
                return false;
            }
        }
        self.apply(
            pubkey,
            parse_status(status),
            created_at,
            PresenceSource::Snapshot,
        )
    }

    fn apply(
        &mut self,
        pubkey: &str,
        state: Presence,
        created_at: i64,
        source: PresenceSource,
    ) -> bool {
        let record = PresenceRecord {
            state,
            last_seen: Some(created_at),
            source,
        };
        let changed = self
            .records
            .get(pubkey)
            .is_none_or(|existing| existing.state != record.state);
        self.records.insert(pubkey.to_string(), record);
        changed
    }

    /// The presence of one pubkey as of `now`.
    ///
    /// A `present` record whose beat has aged past [`PRESENCE_BEAT_TTL_SECS`]
    /// lapses back to **`unknown`**, not `offline`: beats stopping means the
    /// daemon stopped hearing, which is not the same as the agent saying it
    /// went away. Only an explicit `offline` status is `offline`.
    pub fn get(&self, pubkey: &str, now: i64) -> PresenceRecord {
        let Some(record) = self.records.get(pubkey) else {
            return PresenceRecord::unknown();
        };
        let lapsed = record.source == PresenceSource::Beat
            && record.state == Presence::Present
            && record
                .last_seen
                .is_some_and(|seen| now - seen > PRESENCE_BEAT_TTL_SECS);
        if lapsed {
            return PresenceRecord {
                state: Presence::Unknown,
                ..record.clone()
            };
        }
        record.clone()
    }

    /// The presence map for a set of pubkeys, as `GET /presence` serves it.
    pub fn snapshot(
        &self,
        pubkeys: &[String],
        now: i64,
    ) -> std::collections::BTreeMap<String, PresenceRecord> {
        pubkeys
            .iter()
            .map(|pubkey| (pubkey.clone(), self.get(pubkey, now)))
            .collect()
    }
}

/// Map a wire status string onto the four-state glyph set of §3.1.
///
/// An unrecognized status is **`Unknown`**, not a default: a status the daemon
/// does not understand is not evidence of anything, and guessing `present`
/// would render a live dot for an agent that said something else entirely.
fn parse_status(status: &str) -> Presence {
    match status {
        "online" | "present" | "active" => Presence::Present,
        "away" | "waking" | "starting" => Presence::Waking,
        "offline" => Presence::Offline,
        _ => Presence::Unknown,
    }
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

    const NOW: i64 = 1_785_852_720;
    const PK: &str = "aabbccddee";

    /// §2.4: an unheard-from pubkey is `unknown`, and the record says so about
    /// its source too — "we have not heard" is a fact, not an absence.
    #[test]
    fn nothing_heard_yields_unknown_not_offline() {
        let tracker = PresenceTracker::new();
        let record = tracker.get(PK, NOW);
        assert_eq!(record.state, Presence::Unknown);
        assert_eq!(record.source, PresenceSource::None);
        assert_eq!(record.last_seen, None);
    }

    /// A live beat is the primary source.
    #[test]
    fn a_live_beat_makes_someone_present() {
        let mut tracker = PresenceTracker::new();
        assert!(tracker.observe_beat(PK, "online", NOW));
        let record = tracker.get(PK, NOW);
        assert_eq!(record.state, Presence::Present);
        assert_eq!(record.source, PresenceSource::Beat);
    }

    /// [D-5]: presence is a coalescing topic because most beats change nothing.
    /// Reporting "changed" on every beat would push a frame per beat per agent.
    #[test]
    fn a_repeated_beat_reports_no_change() {
        let mut tracker = PresenceTracker::new();
        assert!(tracker.observe_beat(PK, "online", NOW));
        assert!(!tracker.observe_beat(PK, "online", NOW + 30));
        assert!(
            tracker.observe_beat(PK, "away", NOW + 60),
            "a real transition does report"
        );
    }

    /// §4.1.1 deliverable 11: 40902 is the cold-start source. Without it a
    /// fresh daemon shows the whole fleet as `unknown` until the next beat,
    /// because 20001 is ephemeral and the relay stores nothing.
    #[test]
    fn a_snapshot_supplies_cold_start_presence() {
        let mut tracker = PresenceTracker::new();
        assert!(tracker.observe_snapshot(PK, "online", NOW - 600));
        let record = tracker.get(PK, NOW);
        assert_eq!(record.source, PresenceSource::Snapshot);
        assert_eq!(record.last_seen, Some(NOW - 600));
    }

    /// A stale snapshot must not override newer beat evidence — applying one
    /// would flip a working agent to offline.
    #[test]
    fn a_stale_snapshot_does_not_override_a_newer_beat() {
        let mut tracker = PresenceTracker::new();
        tracker.observe_beat(PK, "online", NOW);
        assert!(!tracker.observe_snapshot(PK, "offline", NOW - 3_600));
        assert_eq!(tracker.get(PK, NOW).state, Presence::Present);
    }

    /// A *newer* snapshot does apply: the agent genuinely published that it
    /// went away.
    #[test]
    fn a_newer_snapshot_does_apply() {
        let mut tracker = PresenceTracker::new();
        tracker.observe_beat(PK, "online", NOW - 100);
        assert!(tracker.observe_snapshot(PK, "offline", NOW));
        assert_eq!(tracker.get(PK, NOW).state, Presence::Offline);
    }

    /// **Beats stopping is not the agent saying it left.** A lapsed beat
    /// returns to `unknown`; only an explicit `offline` status is `offline`.
    /// Collapsing the two would report a network partition as a shutdown.
    #[test]
    fn a_lapsed_beat_returns_to_unknown_not_offline() {
        let mut tracker = PresenceTracker::new();
        tracker.observe_beat(PK, "online", NOW);
        assert_eq!(
            tracker.get(PK, NOW + PRESENCE_BEAT_TTL_SECS).state,
            Presence::Present
        );
        assert_eq!(
            tracker.get(PK, NOW + PRESENCE_BEAT_TTL_SECS + 1).state,
            Presence::Unknown,
            "beats stopping is not the same as the agent saying it went away"
        );
    }

    /// An explicitly-published `offline` does not lapse — it is a statement,
    /// not an inference, and it stays true until something newer arrives.
    #[test]
    fn an_explicit_offline_does_not_lapse() {
        let mut tracker = PresenceTracker::new();
        tracker.observe_beat(PK, "offline", NOW);
        assert_eq!(
            tracker.get(PK, NOW + 10_000).state,
            Presence::Offline,
            "an explicit statement is not an aging inference"
        );
    }

    /// An unrecognized status is not evidence of anything. Guessing `present`
    /// would render a live dot for an agent that said something else.
    #[test]
    fn an_unrecognized_status_is_unknown_not_present() {
        let mut tracker = PresenceTracker::new();
        tracker.observe_beat(PK, "hibernating", NOW);
        assert_eq!(tracker.get(PK, NOW).state, Presence::Unknown);
    }

    /// §2.4's global invariant on both presence filters.
    #[test]
    fn both_presence_filters_carry_explicit_kinds() {
        let pubkeys = vec![PK.to_string()];
        for filter in [build_beat_filter(&pubkeys), build_snapshot_filter(&pubkeys)] {
            crate::search::assert_explicit_kinds(&filter, "presence").unwrap();
        }
        assert_eq!(
            build_beat_filter(&pubkeys)["kinds"],
            serde_json::json!([KIND_PRESENCE_BEAT])
        );
        assert_eq!(
            build_snapshot_filter(&pubkeys)["kinds"],
            serde_json::json!([KIND_PRESENCE_SNAPSHOT])
        );
    }

    /// `GET /presence` answers for every pubkey asked about, including ones it
    /// has never heard of — an absent key in the response map is indistinguishable
    /// from a dropped request.
    #[test]
    fn the_snapshot_answers_for_unheard_of_pubkeys_too() {
        let mut tracker = PresenceTracker::new();
        tracker.observe_beat(PK, "online", NOW);
        let map = tracker.snapshot(&[PK.to_string(), "never-seen".to_string()], NOW);
        assert_eq!(map.len(), 2);
        assert_eq!(map["never-seen"].state, Presence::Unknown);
    }
}
