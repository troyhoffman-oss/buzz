//! Observer pipeline: kind-24200 frames, the nine-guard chain, and the archive.
//!
//! Implements `DESIGN.md` §2.4 [D-3], §2.5 ("Observer decryption"), and Wave-1
//! daemon deliverable 8 (§4.1.1).
//!
//! The daemon needs the owner secret key and nothing else — NIP-44's ECDH is
//! symmetric over the pair and the agent half rides on the event. There is no
//! key registry, no provisioning, no per-agent secret.
//!
//! # Ciphertext at rest [D-3]
//!
//! Kind-24200 payloads are the richest plaintext on the box: full prompts,
//! system prompts, file contents, shell output, tool arguments. Decrypt is a
//! pure function of (owner key, event), so storing ciphertext costs one ECDH
//! per read and nothing else. The SQLite file is still `0600` in a `0700`
//! directory; ciphertext-at-rest is defense in depth, not a substitute.
//!
//! # The cache ports the desktop's *separation*, not its number
//!
//! `observerRelayStore.ts` keeps two structures on purpose: a live ring capped
//! at 3000 frames per agent, and a **distinct** channel-scoped archive journal
//! that grows only by explicit paged loads. Collapsing both into one LRU
//! inherits neither property, and the arithmetic is worse than it looks: 32
//! concurrent agents × 3000 frames × [`OBSERVER_MAX_PLAINTEXT_LEN`] is a ~6 GB
//! worst case. So the live ring is per-agent and frame-counted, archive reads
//! stream from SQLite and never populate the ring, and the **decrypt cache is
//! sized in bytes** ([`crate::config::DEFAULT_OBSERVER_CACHE_BYTES`]) evicted
//! LRU.

use serde::{Deserialize, Serialize};

/// Live-ring capacity per agent, matching the desktop's
/// `MAX_OBSERVER_EVENTS` in `observerRelayStore.ts` ([D-3]).
pub const MAX_OBSERVER_EVENTS: usize = 3_000;

/// Maximum accepted decrypted plaintext length (guard 7).
pub const OBSERVER_MAX_PLAINTEXT_LEN: usize = 65_535;

/// Ciphertext length window accepted by guard 5.
pub const NIP44_CIPHERTEXT_LEN: std::ops::RangeInclusive<usize> = 132..=87_472;

/// Freshness / anti-replay window for guard 0, in seconds.
///
/// §2.5: **guard 0 is not optional.** Kind 24200 is ephemeral, so the relay
/// never stores it and there is no relay-side dedup to fall back on. Anyone who
/// can capture a frame can replay it indefinitely, and the guard-8 dedup does
/// not save you: a replay carries the same `(agent_pubkey, seq, timestamp)`
/// triple and is deduped only while the daemon still holds that window; after
/// eviction it re-enters. Both sides of the harness already apply this window —
/// `crates/buzz-relay/src/handlers/event.rs` rejects observer frames outside
/// ±300 s and `crates/buzz-acp/src/lib.rs` applies the identical
/// `OBSERVER_CONTROL_FRESHNESS_SECS = 300`. The daemon is the third party that
/// needs it.
pub const OBSERVER_FRESHNESS_SECS: i64 = 300;

/// Capacity of the pending-unknown-agent queue ([D-3]).
///
/// Guard 4 rejects a frame whose pubkey is not in the agent registry — but on
/// cold start the registry has not loaded yet, and dropping there is the
/// difference between "the agent feed works" and "the agent feed is empty until
/// you restart". The desktop buffers up to
/// `MAX_PENDING_UNKNOWN_AGENT_FRAMES = 100` pending frames and re-evaluates
/// them when the registry arrives; the daemon does the same, with its own
/// counter for frames evicted from that queue.
pub const MAX_PENDING_UNKNOWN_AGENT_FRAMES: usize = 100;

/// The nine guards every frame runs before it can reach any client (§2.5).
///
/// Guards 1–2 come from the desktop's `decrypt_observer_event`; guards 3–4 come
/// from `observerRelayStore.ts`'s application-level check; guard 4 buffers
/// before it drops, per [D-3]. All nine belong in the daemon, and **every
/// counter is exposed on `GET /daemon`** so "why is this agent's feed empty"
/// has an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Guard {
    /// 0. `|event.created_at − now| ≤ 300 s` — freshness / anti-replay.
    Freshness,
    /// 1. `event.verify_id()`.
    EventId,
    /// 2. `event.verify_signature()`.
    Signature,
    /// 3. `event.pubkey == tags["agent"]` — the sender is who it claims.
    AgentTagMatchesPubkey,
    /// 4. that pubkey ∈ known agent registry — **queue, then** drop + count.
    KnownAgent,
    /// 5. `content_looks_like_nip44` — see [`NIP44_CIPHERTEXT_LEN`].
    CiphertextShape,
    /// 6. `nip44::decrypt(owner_secret, …)`.
    Decrypt,
    /// 7. `plaintext ≤ 65_535`.
    PlaintextLength,
    /// 8. dedup on `(agent_pubkey, seq, timestamp)`.
    Dedup,
}

impl Guard {
    /// All nine guards in evaluation order.
    pub const ALL: [Guard; 9] = [
        Guard::Freshness,
        Guard::EventId,
        Guard::Signature,
        Guard::AgentTagMatchesPubkey,
        Guard::KnownAgent,
        Guard::CiphertextShape,
        Guard::Decrypt,
        Guard::PlaintextLength,
        Guard::Dedup,
    ];

    /// Counter name exposed on `GET /daemon`.
    pub fn counter_name(self) -> &'static str {
        match self {
            Guard::Freshness => "observer_dropped_stale",
            Guard::EventId => "observer_dropped_bad_id",
            Guard::Signature => "observer_dropped_bad_sig",
            Guard::AgentTagMatchesPubkey => "observer_dropped_agent_mismatch",
            Guard::KnownAgent => "observer_dropped_unknown_agent",
            Guard::CiphertextShape => "observer_dropped_bad_ciphertext",
            Guard::Decrypt => "observer_dropped_decrypt_failed",
            Guard::PlaintextLength => "observer_dropped_oversize_plaintext",
            Guard::Dedup => "observer_dropped_duplicate",
        }
    }
}

/// Archive identity of a frame ([D-3]).
///
/// §2.5: the archive schema makes `(agent_pubkey, seq, timestamp)` the frame's
/// archive identity with an **idempotent upsert**, so even a within-window
/// replay is a no-op rather than a duplicate row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameKey {
    /// Agent pubkey, lowercase hex.
    pub agent_pubkey: String,
    /// Frame sequence number as reported by the agent.
    pub seq: u64,
    /// Frame timestamp, unix seconds.
    pub timestamp: i64,
}

/// Guard 6: NIP-44 decrypt with the owner secret.
///
/// §2.5: "the daemon needs the owner secret key and nothing else — NIP-44's
/// ECDH is symmetric over the pair and the agent half rides on the event. There
/// is no key registry, no provisioning, no per-agent secret."
///
/// This is the seam where key material meets frame data, and it is deliberately
/// the *only* one: [`crate::identity::Identity`]'s secret accessor is
/// crate-internal so no caller outside the daemon can reach it.
///
/// TODO(wave1, §4.1.1 deliverable 8): perform the real
/// `nostr::nips::nip44::decrypt(owner_secret, agent_pubkey, ciphertext)`. The
/// keyless precondition below is already load-bearing: a daemon running without
/// an identity reports `archiving: false` on `/health` (§2.5), and a decrypt
/// attempted in that state must fail as a counted guard rejection rather than
/// panicking or silently yielding an empty frame.
pub fn decrypt_frame(
    identity: &crate::identity::Identity,
    _agent_pubkey: &str,
    ciphertext: &str,
) -> std::result::Result<String, Guard> {
    if identity.secret_bytes().is_empty() {
        // Keyless: no secret was ever loaded. Counted as a decrypt failure so
        // `GET /daemon` can answer "why is this agent's feed empty".
        return Err(Guard::Decrypt);
    }
    if !content_looks_like_nip44(ciphertext) {
        return Err(Guard::CiphertextShape);
    }
    Err(Guard::Decrypt)
}

/// Guard 0: is this frame inside the ±300 s freshness window?
pub fn is_fresh(created_at: i64, now: i64) -> bool {
    (created_at - now).abs() <= OBSERVER_FRESHNESS_SECS
}

/// Guard 5: does this content have the shape of a NIP-44 payload?
pub fn content_looks_like_nip44(content: &str) -> bool {
    NIP44_CIPHERTEXT_LEN.contains(&content.len())
}

/// Guard 7: is the decrypted plaintext within bounds?
pub fn plaintext_within_bounds(plaintext: &str) -> bool {
    plaintext.len() <= OBSERVER_MAX_PLAINTEXT_LEN
}

/// Per-guard drop accounting, exposed on `GET /daemon` (§2.5).
///
/// Wave-1 exit criterion 5 (§4.1.4) reads these: they must be zero across a
/// normal working day, and any non-zero value must have an explained cause.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GuardCounters {
    /// One counter per guard, keyed by [`Guard::counter_name`].
    pub dropped: std::collections::BTreeMap<String, u64>,
    /// Frames evicted from the pending-unknown-agent queue ([D-3]) — distinct
    /// from a guard-4 drop, because the frame was never evaluated against a
    /// loaded registry.
    pub pending_unknown_evicted: u64,
}

impl GuardCounters {
    /// Record a drop against `guard`.
    pub fn record(&mut self, guard: Guard) {
        *self
            .dropped
            .entry(guard.counter_name().to_string())
            .or_insert(0) += 1;
    }

    /// Total drops across every guard.
    pub fn total(&self) -> u64 {
        self.dropped.values().sum()
    }
}

/// The observer pipeline.
///
/// TODO(wave1, §4.1.1 deliverable 8): implement, in order:
/// 1. the 24200 subscription with the reference filter (`limit 1000`,
///    `since now-300`);
/// 2. the nine-guard chain of [`Guard`], each rejection incrementing its own
///    [`GuardCounters`] entry;
/// 3. ciphertext-at-rest archive with an idempotent [`FrameKey`] upsert [D-3];
/// 4. the byte-budgeted decrypt cache plus separate live-ring/archive paths and
///    the [`MAX_PENDING_UNKNOWN_AGENT_FRAMES`] queue [D-3];
/// 5. `GET /agent/{pk}/activity` merging live + archived into **one** sorted
///    deduplicated sequence;
/// 6. `GET /agent/{pk}/transcript` doing the ACP fold in the daemon;
/// 7. `POST /agent/{pk}/control` for **cancel-turn and switch-model only** —
///    see [`ControlPayload`].
#[derive(Debug, Default)]
pub struct ObserverPipeline {
    counters: GuardCounters,
}

impl ObserverPipeline {
    /// A pipeline with zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current drop accounting, as served on `GET /daemon`.
    pub fn counters(&self) -> &GuardCounters {
        &self.counters
    }
}

/// The **only** two control payloads `POST /agent/{pk}/control` accepts in
/// Wave 1 (§2.4).
///
/// This is not a scoping preference, it is what the harness implements:
/// `handle_relay_observer_control_event` (`crates/buzz-acp/src/lib.rs`)
/// dispatches on those two and logs-and-drops everything else. A third control
/// type added on the daemon side would be *silently* discarded by the agent,
/// which is the worst available failure shape. Stated in the type so nobody
/// adds one.
///
/// Permission answering is **not** a control frame — it is
/// `POST /message/{id}/ask`; see [`crate::askcard`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlPayload {
    /// Cancel the agent's current turn.
    CancelTurn,
    /// Switch the agent's model.
    SwitchModel {
        /// Target model identifier.
        model: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.5 specifies nine guards, numbered 0–8.
    #[test]
    fn there_are_nine_guards_each_with_a_distinct_counter() {
        assert_eq!(Guard::ALL.len(), 9);
        let names: std::collections::BTreeSet<_> =
            Guard::ALL.iter().map(|g| g.counter_name()).collect();
        assert_eq!(names.len(), 9, "counter names must be distinct");
    }

    /// Guard 0 is symmetric: a frame from the future is as suspect as a stale
    /// one, because `|created_at − now|` is an absolute value.
    #[test]
    fn freshness_window_is_symmetric_and_inclusive() {
        let now = 1_000_000;
        assert!(is_fresh(now, now));
        assert!(is_fresh(now - OBSERVER_FRESHNESS_SECS, now));
        assert!(is_fresh(now + OBSERVER_FRESHNESS_SECS, now));
        assert!(!is_fresh(now - OBSERVER_FRESHNESS_SECS - 1, now));
        assert!(!is_fresh(now + OBSERVER_FRESHNESS_SECS + 1, now));
    }

    /// §2.5 cites the identical 300 s window on both sides of the harness.
    #[test]
    fn freshness_matches_the_harness_constant() {
        assert_eq!(OBSERVER_FRESHNESS_SECS, 300);
    }

    #[test]
    fn ciphertext_shape_bounds_match_the_design() {
        assert!(!content_looks_like_nip44(&"a".repeat(131)));
        assert!(content_looks_like_nip44(&"a".repeat(132)));
        assert!(content_looks_like_nip44(&"a".repeat(87_472)));
        assert!(!content_looks_like_nip44(&"a".repeat(87_473)));
    }

    #[test]
    fn plaintext_bound_is_inclusive() {
        assert!(plaintext_within_bounds(
            &"a".repeat(OBSERVER_MAX_PLAINTEXT_LEN)
        ));
        assert!(!plaintext_within_bounds(
            &"a".repeat(OBSERVER_MAX_PLAINTEXT_LEN + 1)
        ));
    }

    /// [D-3]: the live ring matches the desktop's number.
    #[test]
    fn live_ring_matches_the_desktop() {
        assert_eq!(MAX_OBSERVER_EVENTS, 3_000);
        assert_eq!(MAX_PENDING_UNKNOWN_AGENT_FRAMES, 100);
    }

    /// §4.1.4 exit criterion 5 reads these counters, so each guard must move
    /// its own and only its own.
    #[test]
    fn counters_are_per_guard() {
        let mut counters = GuardCounters::default();
        counters.record(Guard::Freshness);
        counters.record(Guard::Freshness);
        counters.record(Guard::Decrypt);
        assert_eq!(counters.total(), 3);
        assert_eq!(counters.dropped["observer_dropped_stale"], 2);
        assert_eq!(counters.dropped["observer_dropped_decrypt_failed"], 1);
    }

    /// §2.4: exactly two control payloads exist in Wave 1. A third would be
    /// silently discarded by the agent.
    #[test]
    fn control_payloads_are_exactly_cancel_and_switch_model() {
        let cancel = serde_json::to_string(&ControlPayload::CancelTurn).unwrap();
        assert_eq!(cancel, r#"{"type":"cancel_turn"}"#);
        let switch = serde_json::to_string(&ControlPayload::SwitchModel {
            model: "claude-opus-5".into(),
        })
        .unwrap();
        assert!(switch.contains(r#""type":"switch_model""#), "{switch}");
        // A payload the harness does not dispatch on must fail to deserialize
        // rather than reach the agent and be dropped there.
        assert!(serde_json::from_str::<ControlPayload>(r#"{"type":"restart"}"#).is_err());
    }

    /// §2.5: a keyless daemon fails decrypt as a **counted guard rejection**,
    /// never as a panic and never as a silently empty frame — `GET /daemon`
    /// must be able to answer "why is this agent's feed empty".
    #[test]
    fn keyless_decrypt_is_a_counted_guard_rejection() {
        use zeroize::Zeroizing;
        let keyless =
            crate::identity::Identity::new("aa".repeat(32), Zeroizing::new(Vec::new()), None);
        let err = decrypt_frame(&keyless, &"bb".repeat(32), &"c".repeat(200)).unwrap_err();
        assert_eq!(err, Guard::Decrypt);
    }

    /// Guard 5 runs before guard 6, so a mis-shaped payload is attributed to
    /// the ciphertext-shape counter rather than to decrypt.
    #[test]
    fn misshaped_ciphertext_is_attributed_to_guard_five() {
        use zeroize::Zeroizing;
        let keyed =
            crate::identity::Identity::new("aa".repeat(32), Zeroizing::new(vec![7u8; 32]), None);
        let err = decrypt_frame(&keyed, &"bb".repeat(32), "too-short").unwrap_err();
        assert_eq!(err, Guard::CiphertextShape);
    }

    /// [D-3]: the archive identity is the triple, so a within-window replay
    /// upserts rather than duplicating.
    #[test]
    fn frame_key_is_the_archive_identity() {
        let a = FrameKey {
            agent_pubkey: "aa".repeat(32),
            seq: 7,
            timestamp: 1_700_000_000,
        };
        let replay = a.clone();
        assert_eq!(a, replay);
        let later = FrameKey { seq: 8, ..a };
        assert_ne!(later, replay);
    }
}
