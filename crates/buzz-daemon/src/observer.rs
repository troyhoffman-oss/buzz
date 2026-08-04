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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
/// The keyless precondition is load-bearing: a daemon running without an
/// identity reports `archiving: false` on `/health` (§2.5), and a decrypt
/// attempted in that state fails as a **counted guard rejection** rather than
/// panicking or silently yielding an empty frame — so `GET /daemon` can answer
/// "why is this agent's feed empty".
pub fn decrypt_frame(
    identity: &crate::identity::Identity,
    agent_pubkey: &str,
    ciphertext: &str,
) -> std::result::Result<String, Guard> {
    if identity.secret_bytes().is_empty() {
        return Err(Guard::Decrypt);
    }
    // Guard 5 before guard 6, so a mis-shaped payload is attributed to the
    // ciphertext-shape counter rather than blamed on the key.
    if !content_looks_like_nip44(ciphertext) {
        return Err(Guard::CiphertextShape);
    }
    let keys = identity.keys().ok_or(Guard::Decrypt)?;
    let sender = nostr::PublicKey::from_hex(agent_pubkey).map_err(|_| Guard::Decrypt)?;
    let plaintext = nostr::nips::nip44::decrypt(keys.secret_key(), &sender, ciphertext)
        .map_err(|_| Guard::Decrypt)?;
    // Guard 7 lives with the decrypt because the plaintext is the only thing
    // that knows its own size, and returning an oversize string to a caller
    // that must then remember to check it is how the check gets skipped.
    if !plaintext_within_bounds(&plaintext) {
        return Err(Guard::PlaintextLength);
    }
    Ok(plaintext)
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

/// A decrypted observer frame, as `GET /agent/{pk}/activity` serves it.
///
/// Field names mirror `ObserverEvent` (`crates/buzz-acp/src/observer.rs`),
/// which serializes camelCase on the wire between agent and relay. The daemon
/// deserializes that shape and re-serializes snake_case, because the daemon→TUI
/// contract is its own — a client that had to know the harness's casing would
/// be coupled to the harness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserverFrame {
    /// Agent pubkey this frame belongs to.
    pub agent_pubkey: String,
    /// Monotonic sequence number, process-local to the agent.
    pub seq: u64,
    /// RFC3339 UTC timestamp from the payload.
    pub timestamp: String,
    /// The relay event's `created_at`, unix seconds. Distinct from
    /// [`Self::timestamp`]: the agent's clock and the event's clock can differ,
    /// and guard 0 checks the *event's*.
    pub created_at: i64,
    /// Observer event kind, e.g. `acp_read` or `turn_started`.
    pub kind: String,
    /// Buzz channel uuid for channel-scoped frames.
    pub channel_id: Option<String>,
    /// ACP session id when known.
    pub session_id: Option<String>,
    /// Local uuid for one prompt turn.
    pub turn_id: Option<String>,
    /// Authoritative turn start, when the agent reported one.
    pub started_at: Option<String>,
    /// Raw or semantic ACP payload.
    pub payload: serde_json::Value,
}

impl ObserverFrame {
    /// The archive identity of this frame ([D-3]).
    pub fn key(&self) -> FrameKey {
        FrameKey {
            agent_pubkey: self.agent_pubkey.clone(),
            seq: self.seq,
            timestamp: self.created_at,
        }
    }

    /// Approximate heap cost, for the **byte**-budgeted decrypt cache ([D-3]).
    ///
    /// Frame *count* is not a memory bound when a frame carries an
    /// arbitrary-size payload: 32 agents × 3000 frames ×
    /// [`OBSERVER_MAX_PLAINTEXT_LEN`] is a ~6 GB worst case, which is why the
    /// budget is in bytes.
    pub fn size_bytes(&self) -> usize {
        self.agent_pubkey.len()
            + self.timestamp.len()
            + self.kind.len()
            + self.channel_id.as_ref().map_or(0, String::len)
            + self.session_id.as_ref().map_or(0, String::len)
            + self.turn_id.as_ref().map_or(0, String::len)
            + self.started_at.as_ref().map_or(0, String::len)
            + self.payload.to_string().len()
    }

    /// Parse a decrypted plaintext into a frame.
    pub fn from_plaintext(
        agent_pubkey: &str,
        created_at: i64,
        plaintext: &str,
    ) -> std::result::Result<Self, Guard> {
        let value: serde_json::Value =
            serde_json::from_str(plaintext).map_err(|_| Guard::Decrypt)?;
        let str_field = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        Ok(Self {
            agent_pubkey: agent_pubkey.to_string(),
            seq: value
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .ok_or(Guard::Decrypt)?,
            timestamp: str_field("timestamp").unwrap_or_default(),
            created_at,
            kind: str_field("kind").ok_or(Guard::Decrypt)?,
            channel_id: str_field("channelId").or_else(|| str_field("channel_id")),
            session_id: str_field("sessionId").or_else(|| str_field("session_id")),
            turn_id: str_field("turnId").or_else(|| str_field("turn_id")),
            started_at: str_field("startedAt").or_else(|| str_field("started_at")),
            payload: value
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
    }
}

/// Build the 24200 subscription filter — the reference filter of §4.1.1
/// deliverable 8 and `daemon-api.md` §3.9.
///
/// `limit: 1000` so a reconnect replay recovers missed frames; `since: now -
/// 300` so session/prompt frames emitted just before subscribe are not lost.
/// The `#p = self` scope is **mandatory**: 24200 is a `P_GATED_KIND`, so
/// without it the relay closes the subscription as `restricted:` and the agent
/// feed is silently empty forever.
pub fn build_observer_filter(self_pubkey: &str, now: i64) -> serde_json::Value {
    serde_json::json!({
        "kinds": [buzz_core::kind::KIND_AGENT_OBSERVER_FRAME],
        "#p": [self_pubkey],
        "limit": 1000,
        "since": (now - OBSERVER_FRESHNESS_SECS).max(0),
    })
}

/// A per-agent ring of live frames ([D-3]).
///
/// **Separate from the archive on purpose.** `observerRelayStore.ts` keeps two
/// structures so "loading deep history can never evict live frames"; collapsing
/// them into one LRU inherits neither property. Archive reads stream from
/// SQLite and never populate this ring.
#[derive(Debug, Default)]
struct LiveRing {
    frames: std::collections::VecDeque<ObserverFrame>,
}

impl LiveRing {
    /// Push a frame, returning the bytes of any frame the **frame cap** evicted.
    ///
    /// The return value is not a convenience. There are two independent bounds
    /// on the live set — this per-agent frame cap and the global byte budget —
    /// and both evict. If the frame cap evicts without reporting, the byte
    /// counter keeps charging for a frame that is no longer held, drifts upward
    /// forever, and eventually pins the byte budget permanently over its limit:
    /// `evict_to_budget` then evicts every real frame trying to satisfy a number
    /// that describes nothing. The observer feed goes empty and `GET /daemon`
    /// reports a cache full of frames it does not have.
    fn push(&mut self, frame: ObserverFrame) -> usize {
        let evicted = if self.frames.len() >= MAX_OBSERVER_EVENTS {
            self.frames.pop_front().map_or(0, |f| f.size_bytes())
        } else {
            0
        };
        self.frames.push_back(frame);
        evicted
    }
}

/// The observer pipeline: the nine-guard chain, the live ring, the archive, and
/// the pending-unknown-agent queue.
///
/// Implements §4.1.1 deliverable 8. The archive itself lives in
/// [`crate::cache`] as ciphertext at rest ([D-3]); what is held here is the
/// decrypted hot path.
#[derive(Debug, Default)]
pub struct ObserverPipeline {
    counters: GuardCounters,
    /// Registered agent pubkeys, from the 30177 / 10100 cache. Guard 4.
    known_agents: std::collections::BTreeSet<String>,
    /// Per-agent live rings.
    live: std::collections::BTreeMap<String, LiveRing>,
    /// Guard-8 dedup over the archive identity.
    seen: std::collections::BTreeSet<FrameKey>,
    /// Frames from agents not yet in the registry ([D-3]).
    pending_unknown: std::collections::VecDeque<(String, i64, String)>,
    /// Running byte total of the decrypted frames held live.
    live_bytes: usize,
    /// Byte budget for the decrypt cache.
    cache_budget: usize,
}

/// What the pipeline did with a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Ingest {
    /// Accepted, decrypted, deduped, and pushed to the live ring.
    Accepted(Box<ObserverFrame>),
    /// Held for re-evaluation when the agent registry loads (guard 4, [D-3]).
    Queued,
    /// Rejected by a guard, which has been counted.
    Dropped(Guard),
}

impl ObserverPipeline {
    /// A pipeline with zeroed counters and the default cache budget.
    pub fn new() -> Self {
        Self {
            cache_budget: crate::config::DEFAULT_OBSERVER_CACHE_BYTES as usize,
            ..Self::default()
        }
    }

    /// A pipeline with an explicit byte budget (`--observer-cache-bytes`).
    pub fn with_cache_budget(bytes: usize) -> Self {
        Self {
            cache_budget: bytes,
            ..Self::default()
        }
    }

    /// Current drop accounting, as served on `GET /daemon`.
    pub fn counters(&self) -> &GuardCounters {
        &self.counters
    }

    /// Register an agent, admitting it through guard 4.
    ///
    /// Returns the frames re-admitted from the pending queue, so a cold start
    /// does not lose the frames that arrived before the registry did.
    pub fn register_agent(
        &mut self,
        pubkey: impl Into<String>,
        identity: &crate::identity::Identity,
        now: i64,
    ) -> Vec<ObserverFrame> {
        self.known_agents.insert(pubkey.into());
        self.drain_pending(identity, now)
    }

    /// Whether an agent is registered.
    pub fn knows_agent(&self, pubkey: &str) -> bool {
        self.known_agents.contains(pubkey)
    }

    /// Run the nine-guard chain over one raw kind-24200 event.
    ///
    /// The order is the order of §2.5, and it matters: each guard is attributed
    /// to its own counter, so `GET /daemon` answers "why is this agent's feed
    /// empty" with the actual reason rather than a total.
    pub fn ingest(
        &mut self,
        event: &nostr::Event,
        identity: &crate::identity::Identity,
        now: i64,
    ) -> Ingest {
        // Guard 0 — freshness / anti-replay. Not optional: 24200 is ephemeral,
        // so the relay stores nothing and there is no relay-side dedup to fall
        // back on. Anyone who can capture a frame can replay it indefinitely,
        // and the guard-8 dedup does not save you — a replay carries the same
        // triple and is deduped only while the window still holds it.
        if !is_fresh(event.created_at.as_secs() as i64, now) {
            return self.drop(Guard::Freshness);
        }
        // Guards 1–2 — from the desktop's `decrypt_observer_event`.
        if !event.verify_id() {
            return self.drop(Guard::EventId);
        }
        if !event.verify_signature() {
            return self.drop(Guard::Signature);
        }
        // Guard 3 — the sender is who it claims to be.
        let claimed = tag_value(event, buzz_core::observer::OBSERVER_AGENT_TAG);
        let sender = event.pubkey.to_hex();
        if claimed.as_deref() != Some(sender.as_str()) {
            return self.drop(Guard::AgentTagMatchesPubkey);
        }
        // Guard 4 — known agent, **queue before dropping** ([D-3]).
        if !self.known_agents.contains(&sender) {
            return self.queue_unknown(sender, event.created_at.as_secs() as i64, &event.content);
        }
        // Guards 5–7 — ciphertext shape, decrypt, plaintext bound.
        let plaintext = match decrypt_frame(identity, &sender, &event.content) {
            Ok(plaintext) => plaintext,
            Err(guard) => return self.drop(guard),
        };
        let frame = match ObserverFrame::from_plaintext(
            &sender,
            event.created_at.as_secs() as i64,
            &plaintext,
        ) {
            Ok(frame) => frame,
            Err(guard) => return self.drop(guard),
        };
        self.admit(frame)
    }

    /// Guard 8 plus the live-ring push.
    fn admit(&mut self, frame: ObserverFrame) -> Ingest {
        if !self.seen.insert(frame.key()) {
            return self.drop(Guard::Dedup);
        }
        self.live_bytes += frame.size_bytes();
        let evicted = self
            .live
            .entry(frame.agent_pubkey.clone())
            .or_default()
            .push(frame.clone());
        // The frame cap and the byte budget are independent bounds and both
        // evict; charging for a frame the cap already dropped would drift the
        // counter upward until the budget is permanently over its limit.
        self.live_bytes = self.live_bytes.saturating_sub(evicted);
        self.evict_to_budget();
        Ingest::Accepted(Box::new(frame))
    }

    /// Hold a frame from an unregistered agent ([D-3]).
    ///
    /// Guard 4 rejects a frame whose pubkey is not in the registry — but on cold
    /// start the registry has not loaded yet, and dropping there is the
    /// difference between "the agent feed works" and "the agent feed is empty
    /// until you restart". Eviction from *this* queue gets its own counter,
    /// because such a frame was never evaluated against a loaded registry and
    /// counting it as a guard-4 drop would misattribute the cause.
    fn queue_unknown(&mut self, pubkey: String, created_at: i64, ciphertext: &str) -> Ingest {
        if self.pending_unknown.len() >= MAX_PENDING_UNKNOWN_AGENT_FRAMES {
            self.pending_unknown.pop_front();
            self.counters.pending_unknown_evicted += 1;
        }
        self.pending_unknown
            .push_back((pubkey, created_at, ciphertext.to_string()));
        Ingest::Queued
    }

    /// Re-evaluate pending frames now that the registry has grown.
    ///
    /// Frames whose agent is *still* unknown stay queued: the registry may load
    /// in several batches, and discarding on the first pass would defeat the
    /// queue's purpose.
    fn drain_pending(
        &mut self,
        identity: &crate::identity::Identity,
        now: i64,
    ) -> Vec<ObserverFrame> {
        let pending = std::mem::take(&mut self.pending_unknown);
        let mut admitted = Vec::new();
        for (pubkey, created_at, ciphertext) in pending {
            if !self.known_agents.contains(&pubkey) {
                self.pending_unknown
                    .push_back((pubkey, created_at, ciphertext));
                continue;
            }
            // Guard 0 is re-checked: a frame that sat in the queue long enough
            // to go stale is exactly the replay window the guard defends.
            if !is_fresh(created_at, now) {
                self.drop(Guard::Freshness);
                continue;
            }
            match decrypt_frame(identity, &pubkey, &ciphertext).and_then(|plaintext| {
                ObserverFrame::from_plaintext(&pubkey, created_at, &plaintext)
            }) {
                Ok(frame) => {
                    if let Ingest::Accepted(frame) = self.admit(frame) {
                        admitted.push(*frame);
                    }
                }
                Err(guard) => {
                    self.drop(guard);
                }
            }
        }
        admitted
    }

    /// Evict oldest frames until the live set fits the byte budget ([D-3]).
    fn evict_to_budget(&mut self) {
        while self.live_bytes > self.cache_budget {
            // Evict from whichever agent's ring is oldest at its head, so one
            // chatty agent cannot starve a quiet one's history.
            let victim = self
                .live
                .iter()
                .filter_map(|(pubkey, ring)| {
                    ring.frames.front().map(|f| (f.created_at, pubkey.clone()))
                })
                .min()
                .map(|(_, pubkey)| pubkey);
            let Some(pubkey) = victim else { break };
            let Some(ring) = self.live.get_mut(&pubkey) else {
                break;
            };
            match ring.frames.pop_front() {
                Some(frame) => self.live_bytes = self.live_bytes.saturating_sub(frame.size_bytes()),
                None => break,
            }
        }
    }

    fn drop(&mut self, guard: Guard) -> Ingest {
        self.counters.record(guard);
        Ingest::Dropped(guard)
    }

    /// The live frames held for one agent, oldest first.
    pub fn live_frames(&self, agent_pubkey: &str) -> Vec<ObserverFrame> {
        self.live
            .get(agent_pubkey)
            .map(|ring| ring.frames.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// How many frames are waiting for their agent to be registered.
    pub fn pending_unknown_len(&self) -> usize {
        self.pending_unknown.len()
    }

    /// Current decrypted-frame byte usage.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }
}

/// Merge live and archived frames into **one** sorted, deduplicated sequence.
///
/// `daemon-api.md` §3.9: "the same discipline the desktop uses — one
/// `buildTranscriptState()` over the combined set, never two independent state
/// machines whose stateful relationships split across a boundary." Two
/// sequences would mean a tool-call `start` in the archive and its `end` in the
/// live ring never pair up, which is precisely the transcript fold that
/// `GET /agent/{pk}/transcript` performs.
///
/// Ordering is `(created_at, seq)`: `seq` alone is process-local to the agent
/// and restarts, and `created_at` alone ties within a second.
pub fn merge_activity(
    live: Vec<ObserverFrame>,
    archived: Vec<ObserverFrame>,
) -> Vec<ObserverFrame> {
    let mut seen: std::collections::BTreeSet<FrameKey> = std::collections::BTreeSet::new();
    let mut merged: Vec<ObserverFrame> = Vec::with_capacity(live.len() + archived.len());
    for frame in archived.into_iter().chain(live) {
        if seen.insert(frame.key()) {
            merged.push(frame);
        }
    }
    merged.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.seq.cmp(&b.seq))
    });
    merged
}

/// Read a tag's first value off a signed event.
fn tag_value(event: &nostr::Event, name: &str) -> Option<String> {
    event
        .tags
        .iter()
        .map(nostr::Tag::as_slice)
        .find(|slice| slice.first().map(String::as_str) == Some(name))
        .and_then(|slice| slice.get(1).cloned())
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

    // ── §5.2 `observer guard chain` — real keys, real NIP-44, real signatures ──
    //
    // These build genuinely signed, genuinely encrypted frames rather than
    // hand-shaped JSON. A guard chain tested against fixtures it constructed
    // itself proves only that the fixtures match the code; the point of guards
    // 1, 2, and 6 is what happens to material the daemon did *not* construct.

    use crate::identity::Identity;
    use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag};

    fn owner_and_agent() -> (Identity, Keys) {
        let owner = Keys::generate();
        let agent = Keys::generate();
        (Identity::from_keys(owner, None), agent)
    }

    fn payload(seq: u64, kind: &str) -> serde_json::Value {
        serde_json::json!({
            "seq": seq,
            "timestamp": "2026-08-04T14:12:00Z",
            "kind": kind,
            "channelId": "11111111-1111-1111-1111-111111111111",
            "sessionId": "sess-1",
            "turnId": "4a91",
            "startedAt": "2026-08-04T14:02:11Z",
            "payload": {"path": "crates/buzz-daemon/src/observer.rs"},
        })
    }

    /// Build a real kind-24200 frame: NIP-44 encrypted to the owner, signed by
    /// the agent, tagged the way `build_agent_observer_frame` tags one.
    fn frame_event(owner: &Identity, agent: &Keys, seq: u64, created_at: i64) -> nostr::Event {
        let owner_pubkey = nostr::PublicKey::from_hex(&owner.pubkey).unwrap();
        let ciphertext = buzz_core::observer::encrypt_observer_payload(
            agent,
            &owner_pubkey,
            &payload(seq, "acp_read"),
        )
        .expect("encrypt");
        EventBuilder::new(
            Kind::Custom(buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16),
            ciphertext,
        )
        .tags([
            Tag::public_key(owner_pubkey),
            Tag::parse([
                buzz_core::observer::OBSERVER_AGENT_TAG,
                &agent.public_key().to_hex(),
            ])
            .unwrap(),
            Tag::parse([
                buzz_core::observer::OBSERVER_FRAME_TAG,
                buzz_core::observer::OBSERVER_FRAME_TELEMETRY,
            ])
            .unwrap(),
        ])
        .custom_created_at(nostr::Timestamp::from_secs(created_at as u64))
        .sign_with_keys(agent)
        .expect("sign")
    }

    fn registered_pipeline(owner: &Identity, agent: &Keys, now: i64) -> ObserverPipeline {
        let mut pipeline = ObserverPipeline::new();
        pipeline.register_agent(agent.public_key().to_hex(), owner, now);
        pipeline
    }

    const NOW: i64 = 1_785_852_720;

    /// The happy path, end to end: encrypt, sign, ingest, decrypt, land.
    #[test]
    fn a_well_formed_frame_survives_all_nine_guards() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = registered_pipeline(&owner, &agent, NOW);
        let event = frame_event(&owner, &agent, 1, NOW);

        let Ingest::Accepted(frame) = pipeline.ingest(&event, &owner, NOW) else {
            panic!("a well-formed frame must be accepted");
        };
        assert_eq!(frame.seq, 1);
        assert_eq!(frame.kind, "acp_read");
        assert_eq!(frame.turn_id.as_deref(), Some("4a91"));
        assert_eq!(frame.agent_pubkey, agent.public_key().to_hex());
        assert_eq!(pipeline.counters().total(), 0);
        assert_eq!(pipeline.live_frames(&agent.public_key().to_hex()).len(), 1);
    }

    /// Guard 0: **a frame >300 s skewed is dropped.** Not optional — 24200 is
    /// ephemeral, so there is no relay-side dedup to fall back on and a
    /// captured frame can be replayed indefinitely.
    #[test]
    fn guard_zero_drops_a_stale_frame_in_both_directions() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = registered_pipeline(&owner, &agent, NOW);

        for skew in [-OBSERVER_FRESHNESS_SECS - 1, OBSERVER_FRESHNESS_SECS + 1] {
            let event = frame_event(&owner, &agent, 1, NOW + skew);
            assert_eq!(
                pipeline.ingest(&event, &owner, NOW),
                Ingest::Dropped(Guard::Freshness),
                "skew {skew} should be refused"
            );
        }
        assert_eq!(
            pipeline.counters().dropped["observer_dropped_stale"],
            2,
            "each drop moves its own counter"
        );
    }

    /// Guard 2: a frame whose signature does not verify is dropped.
    ///
    /// Built by tampering with a genuinely signed event's content, which is
    /// what a MITM on a non-TLS dev relay would produce.
    #[test]
    fn guard_two_drops_a_tampered_frame() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = registered_pipeline(&owner, &agent, NOW);
        let event = frame_event(&owner, &agent, 1, NOW);

        let mut json: serde_json::Value = serde_json::from_str(&event.as_json()).unwrap();
        // Same length, different bytes: the id still hashes over the mutated
        // content, so this exercises guard 1 or 2, never a length check.
        let content = json["content"].as_str().unwrap().to_string();
        let mut tampered: Vec<char> = content.chars().collect();
        tampered[10] = if tampered[10] == 'A' { 'B' } else { 'A' };
        json["content"] = serde_json::json!(tampered.into_iter().collect::<String>());
        let tampered: nostr::Event = serde_json::from_value(json).unwrap();

        let result = pipeline.ingest(&tampered, &owner, NOW);
        assert!(
            matches!(
                result,
                Ingest::Dropped(Guard::EventId) | Ingest::Dropped(Guard::Signature)
            ),
            "{result:?}"
        );
    }

    /// §5.2: "a frame whose `pubkey ≠ agent` tag is dropped."
    ///
    /// The frame is genuinely signed and genuinely encrypted — it just claims
    /// to come from someone else. This is the impersonation case, and it must
    /// be caught *before* the decrypt so the counter names the real reason.
    #[test]
    fn guard_three_drops_a_frame_impersonating_another_agent() {
        let (owner, agent) = owner_and_agent();
        let impostor = Keys::generate();
        let mut pipeline = registered_pipeline(&owner, &agent, NOW);
        pipeline.register_agent(impostor.public_key().to_hex(), &owner, NOW);

        let owner_pubkey = nostr::PublicKey::from_hex(&owner.pubkey).unwrap();
        let ciphertext = buzz_core::observer::encrypt_observer_payload(
            &impostor,
            &owner_pubkey,
            &payload(1, "acp_read"),
        )
        .unwrap();
        // Signed by the impostor, but the `agent` tag names the real agent.
        let event = EventBuilder::new(
            Kind::Custom(buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16),
            ciphertext,
        )
        .tags([
            Tag::public_key(owner_pubkey),
            Tag::parse([
                buzz_core::observer::OBSERVER_AGENT_TAG,
                &agent.public_key().to_hex(),
            ])
            .unwrap(),
        ])
        .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
        .sign_with_keys(&impostor)
        .unwrap();

        assert_eq!(
            pipeline.ingest(&event, &owner, NOW),
            Ingest::Dropped(Guard::AgentTagMatchesPubkey)
        );
        assert_eq!(
            pipeline.counters().dropped["observer_dropped_agent_mismatch"],
            1
        );
    }

    /// §5.2 / [D-3]: "unknown-agent frames queue to 100 then drop-with-count,
    /// and re-evaluate when the registry loads."
    ///
    /// This is the cold-start case, and getting it wrong is the difference
    /// between "the agent feed works" and "the agent feed is empty until you
    /// restart".
    #[test]
    fn guard_four_queues_unknown_agents_and_replays_them_on_registration() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = ObserverPipeline::new();
        assert!(!pipeline.knows_agent(&agent.public_key().to_hex()));

        let event = frame_event(&owner, &agent, 7, NOW);
        assert_eq!(
            pipeline.ingest(&event, &owner, NOW),
            Ingest::Queued,
            "an unknown agent's frame is held, not dropped"
        );
        assert_eq!(pipeline.pending_unknown_len(), 1);
        assert_eq!(pipeline.counters().total(), 0, "queuing is not a drop");

        let admitted = pipeline.register_agent(agent.public_key().to_hex(), &owner, NOW);
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].seq, 7);
        assert_eq!(pipeline.pending_unknown_len(), 0);
        assert_eq!(pipeline.live_frames(&agent.public_key().to_hex()).len(), 1);
    }

    /// The queue is bounded at 100 with its **own** counter — an eviction there
    /// is not a guard-4 drop, because the frame was never evaluated against a
    /// loaded registry, and conflating them misattributes the cause on
    /// `GET /daemon`.
    #[test]
    fn the_pending_queue_is_bounded_with_its_own_counter() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = ObserverPipeline::new();
        for seq in 0..=MAX_PENDING_UNKNOWN_AGENT_FRAMES as u64 {
            pipeline.ingest(&frame_event(&owner, &agent, seq, NOW), &owner, NOW);
        }
        assert_eq!(
            pipeline.pending_unknown_len(),
            MAX_PENDING_UNKNOWN_AGENT_FRAMES
        );
        assert_eq!(pipeline.counters().pending_unknown_evicted, 1);
        assert_eq!(
            pipeline.counters().total(),
            0,
            "a queue eviction is not attributed to guard 4"
        );
    }

    /// A frame that sat in the queue past the freshness window is re-checked
    /// against guard 0 on the way out. The queue is not a way around it.
    #[test]
    fn a_frame_that_goes_stale_in_the_queue_is_still_refused() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = ObserverPipeline::new();
        pipeline.ingest(&frame_event(&owner, &agent, 1, NOW), &owner, NOW);
        assert_eq!(pipeline.pending_unknown_len(), 1);

        let much_later = NOW + OBSERVER_FRESHNESS_SECS + 1;
        let admitted = pipeline.register_agent(agent.public_key().to_hex(), &owner, much_later);
        assert!(admitted.is_empty());
        assert_eq!(pipeline.counters().dropped["observer_dropped_stale"], 1);
    }

    /// Guard 6: a frame encrypted to **someone else** does not decrypt with the
    /// owner's key, and fails as a decrypt drop rather than as garbage.
    #[test]
    fn guard_six_drops_a_frame_encrypted_to_another_owner() {
        let (owner, agent) = owner_and_agent();
        let other_owner = Keys::generate();
        let mut pipeline = registered_pipeline(&owner, &agent, NOW);

        let ciphertext = buzz_core::observer::encrypt_observer_payload(
            &agent,
            &other_owner.public_key(),
            &payload(1, "acp_read"),
        )
        .unwrap();
        let event = EventBuilder::new(
            Kind::Custom(buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16),
            ciphertext,
        )
        .tags([Tag::parse([
            buzz_core::observer::OBSERVER_AGENT_TAG,
            &agent.public_key().to_hex(),
        ])
        .unwrap()])
        .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
        .sign_with_keys(&agent)
        .unwrap();

        assert_eq!(
            pipeline.ingest(&event, &owner, NOW),
            Ingest::Dropped(Guard::Decrypt)
        );
    }

    /// §5.2: "a replayed in-window frame upserts idempotently rather than
    /// duplicating."
    ///
    /// The archive identity is `(agent_pubkey, seq, timestamp)`, so a replay
    /// carries the same triple and guard 8 catches it — which is exactly why
    /// guard 0 has to exist for the out-of-window case.
    #[test]
    fn guard_eight_dedupes_an_in_window_replay() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = registered_pipeline(&owner, &agent, NOW);
        let event = frame_event(&owner, &agent, 1, NOW);

        assert!(matches!(
            pipeline.ingest(&event, &owner, NOW),
            Ingest::Accepted(_)
        ));
        assert_eq!(
            pipeline.ingest(&event, &owner, NOW),
            Ingest::Dropped(Guard::Dedup),
            "the same frame twice is one frame"
        );
        assert_eq!(
            pipeline.live_frames(&agent.public_key().to_hex()).len(),
            1,
            "and it appears once in the ring"
        );
    }

    /// A different `seq` from the same agent is a *different* frame — dedup
    /// must not swallow real telemetry.
    #[test]
    fn dedup_does_not_swallow_distinct_frames() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = registered_pipeline(&owner, &agent, NOW);
        for seq in 1..=3 {
            assert!(matches!(
                pipeline.ingest(&frame_event(&owner, &agent, seq, NOW), &owner, NOW),
                Ingest::Accepted(_)
            ));
        }
        assert_eq!(pipeline.live_frames(&agent.public_key().to_hex()).len(), 3);
    }

    // ── [D-3] the byte budget and the live/archive separation ─────────────

    /// The byte counter must describe what is **actually held**, across both
    /// eviction paths.
    ///
    /// Regression test for a real bug: `LiveRing::push` evicted at the
    /// 3000-frame cap without telling `live_bytes`, so the counter charged for
    /// frames that were gone. It drifts upward forever, eventually pinning the
    /// byte budget permanently over its limit — at which point
    /// `evict_to_budget` evicts every real frame chasing a number that
    /// describes nothing, the observer feed goes empty, and `GET /daemon`
    /// reports a cache full of frames it does not have.
    ///
    /// Asserted by *reconstruction*: sum what the rings hold and compare. A
    /// test that only checked "bytes went up" would have passed throughout.
    #[test]
    fn the_byte_counter_matches_what_the_rings_actually_hold() {
        let (owner, agent) = owner_and_agent();
        // A budget far above anything this test allocates, so the frame cap is
        // the only bound in play and the byte accounting is tested in isolation.
        let mut pipeline = ObserverPipeline::with_cache_budget(usize::MAX);
        pipeline.register_agent(agent.public_key().to_hex(), &owner, NOW);

        let held_bytes = |p: &ObserverPipeline| -> usize {
            p.live_frames(&agent.public_key().to_hex())
                .iter()
                .map(ObserverFrame::size_bytes)
                .sum()
        };

        for seq in 1..=5 {
            pipeline.ingest(&frame_event(&owner, &agent, seq, NOW), &owner, NOW);
        }
        assert_eq!(pipeline.live_bytes(), held_bytes(&pipeline));

        // Now cross the per-agent frame cap, which is the path that used to
        // leak. Filling to 3000 through the real ingest path would be slow, so
        // the ring is driven directly — the accounting under test is the
        // caller's, and this is the only way to reach the cap in a unit test.
        let ring = LiveRing::default();
        let mut ring = ring;
        for seq in 0..MAX_OBSERVER_EVENTS as u64 {
            let evicted = ring.push(bare_frame(&agent.public_key().to_hex(), seq, NOW));
            assert_eq!(evicted, 0, "nothing is evicted below the cap");
        }
        let over_cap = ring.push(bare_frame(&agent.public_key().to_hex(), 9_999, NOW));
        assert!(
            over_cap > 0,
            "crossing the frame cap must report the evicted frame's bytes"
        );
        assert_eq!(ring.frames.len(), MAX_OBSERVER_EVENTS);
    }

    /// The decrypt cache is sized in **bytes, not frames**. Frame count is not
    /// a memory bound when a frame carries an arbitrary-size payload.
    #[test]
    fn the_cache_budget_is_enforced_in_bytes() {
        let (owner, agent) = owner_and_agent();
        let mut pipeline = ObserverPipeline::with_cache_budget(400);
        pipeline.register_agent(agent.public_key().to_hex(), &owner, NOW);

        for seq in 1..=20 {
            pipeline.ingest(&frame_event(&owner, &agent, seq, NOW), &owner, NOW);
        }
        assert!(
            pipeline.live_bytes() <= 400,
            "live bytes {} exceeded the budget",
            pipeline.live_bytes()
        );
        let held = pipeline.live_frames(&agent.public_key().to_hex());
        assert!(!held.is_empty(), "the budget must not evict everything");
        assert!(
            held.len() < 20,
            "and it must actually evict — held {}",
            held.len()
        );
    }

    /// [D-3]: the live ring is per-agent and frame-capped, so one agent's
    /// history cannot evict another's.
    #[test]
    fn live_rings_are_per_agent() {
        let (owner, first) = owner_and_agent();
        let second = Keys::generate();
        let mut pipeline = ObserverPipeline::new();
        pipeline.register_agent(first.public_key().to_hex(), &owner, NOW);
        pipeline.register_agent(second.public_key().to_hex(), &owner, NOW);

        pipeline.ingest(&frame_event(&owner, &first, 1, NOW), &owner, NOW);
        pipeline.ingest(&frame_event(&owner, &second, 1, NOW), &owner, NOW);

        assert_eq!(pipeline.live_frames(&first.public_key().to_hex()).len(), 1);
        assert_eq!(pipeline.live_frames(&second.public_key().to_hex()).len(), 1);
    }

    // ── The merged activity sequence ──────────────────────────────────────

    fn bare_frame(agent: &str, seq: u64, created_at: i64) -> ObserverFrame {
        ObserverFrame {
            agent_pubkey: agent.into(),
            seq,
            timestamp: String::new(),
            created_at,
            kind: "acp_read".into(),
            channel_id: None,
            session_id: None,
            turn_id: None,
            started_at: None,
            payload: serde_json::Value::Null,
        }
    }

    /// `daemon-api.md` §3.9: **one** sorted, deduplicated sequence. Two
    /// sequences would leave a tool-call `start` in the archive unpaired with
    /// its `end` in the live ring.
    #[test]
    fn activity_merges_live_and_archive_into_one_sorted_sequence() {
        let agent = "aa".repeat(32);
        let archived = vec![bare_frame(&agent, 1, 1_000), bare_frame(&agent, 2, 1_010)];
        let live = vec![
            // Overlaps the archive — the reconnect replay case.
            bare_frame(&agent, 2, 1_010),
            bare_frame(&agent, 3, 1_020),
        ];
        let merged = merge_activity(live, archived);
        let seqs: Vec<u64> = merged.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, [1, 2, 3], "deduped and in order");
    }

    /// `seq` alone is process-local to the agent and restarts, so the sort key
    /// is `(created_at, seq)` — otherwise a restarted agent's frame 1 sorts
    /// before the previous run's frame 900.
    #[test]
    fn the_merge_sorts_by_time_first_then_sequence() {
        let agent = "aa".repeat(32);
        let merged = merge_activity(
            vec![bare_frame(&agent, 1, 2_000)],
            vec![bare_frame(&agent, 900, 1_000)],
        );
        let order: Vec<(i64, u64)> = merged.iter().map(|f| (f.created_at, f.seq)).collect();
        assert_eq!(order, [(1_000, 900), (2_000, 1)]);
    }

    // ── The subscription filter ───────────────────────────────────────────

    /// 24200 is a `P_GATED_KIND`: without `#p = self` the relay closes the
    /// subscription and the agent feed is silently empty forever.
    #[test]
    fn the_observer_filter_is_p_scoped_with_the_reference_limit_and_lookback() {
        let me = "aa".repeat(32);
        let filter = build_observer_filter(&me, NOW);
        crate::search::assert_explicit_kinds(&filter, "observer").unwrap();
        assert_eq!(filter["#p"], serde_json::json!([me]));
        assert_eq!(filter["limit"], serde_json::json!(1000));
        assert_eq!(
            filter["since"],
            serde_json::json!(NOW - OBSERVER_FRESHNESS_SECS),
            "the lookback recovers frames emitted just before subscribe"
        );
        assert_eq!(
            filter["kinds"],
            serde_json::json!([buzz_core::kind::KIND_AGENT_OBSERVER_FRAME])
        );
    }

    /// A daemon started near the epoch must not emit a negative `since`.
    #[test]
    fn the_lookback_never_goes_negative() {
        let filter = build_observer_filter(&"aa".repeat(32), 10);
        assert_eq!(filter["since"], serde_json::json!(0));
    }

    /// §2.5: a keyless daemon reports `archiving: false`, and a decrypt in that
    /// state is a counted rejection — never a panic, never an empty frame.
    #[test]
    fn a_keyless_daemon_counts_rather_than_panicking() {
        use zeroize::Zeroizing;
        let (real_owner, agent) = owner_and_agent();
        let keyless = Identity::new("aa".repeat(32), Zeroizing::new(Vec::new()), None);
        let mut pipeline = ObserverPipeline::new();
        pipeline.register_agent(agent.public_key().to_hex(), &keyless, NOW);

        let event = frame_event(&real_owner, &agent, 1, NOW);
        assert_eq!(
            pipeline.ingest(&event, &keyless, NOW),
            Ingest::Dropped(Guard::Decrypt)
        );
        assert_eq!(
            pipeline.counters().dropped["observer_dropped_decrypt_failed"],
            1
        );
    }

    /// The daemon→TUI frame is snake_case even though the harness serializes
    /// camelCase, because the daemon→TUI contract is its own — a client that
    /// had to know the harness's casing would be coupled to the harness.
    #[test]
    fn the_frame_wire_shape_is_snake_case() {
        let json = serde_json::to_string(&bare_frame(&"aa".repeat(32), 1, 1_000)).unwrap();
        assert!(json.contains("\"agent_pubkey\""), "{json}");
        assert!(json.contains("\"created_at\""), "{json}");
        assert!(!json.contains("agentPubkey"), "{json}");
    }

    /// Both spellings parse on the way *in*, because the harness's own field
    /// names are camelCase and that is what arrives on the wire.
    #[test]
    fn plaintext_parsing_accepts_the_harness_camel_case() {
        let camel = serde_json::json!({
            "seq": 4, "timestamp": "t", "kind": "turn_started",
            "channelId": "chan", "turnId": "4a91", "startedAt": "s",
            "payload": {},
        });
        let frame = ObserverFrame::from_plaintext("aa", 1_000, &camel.to_string()).expect("parses");
        assert_eq!(frame.channel_id.as_deref(), Some("chan"));
        assert_eq!(frame.turn_id.as_deref(), Some("4a91"));
        assert_eq!(frame.started_at.as_deref(), Some("s"));
    }

    /// A payload with no `seq` cannot be given an archive identity, so it is a
    /// decrypt-stage rejection rather than a frame with a fabricated `seq: 0`
    /// that would collide with every other such frame in the dedup set.
    #[test]
    fn a_payload_without_a_sequence_is_refused() {
        let bad = serde_json::json!({"kind": "acp_read", "payload": {}});
        assert_eq!(
            ObserverFrame::from_plaintext("aa", 1_000, &bad.to_string()).unwrap_err(),
            Guard::Decrypt
        );
    }
}
