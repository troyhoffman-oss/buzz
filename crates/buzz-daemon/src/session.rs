//! Relay session layer over [`buzz_ws_client`].
//!
//! Implements `DESIGN.md` §2.4 [D-4], §2.6, and Wave-1 daemon deliverable 1
//! (§4.1.1): NIP-42 auth, a subscription registry with per-channel `since`
//! watermarks, `TwoGenDedup` at 12k, ping/pong half-open detection, the backoff
//! ladder with its DNS-brownout special case, REQ pacing at 125 ms with a drain
//! budget of 1, and the gated-observer park queue at 256 with visible drop
//! accounting.
//!
//! # Why this is a reimplementation and not a fork
//!
//! [D-4] resolves it: `crates/buzz-acp/src/relay.rs` is 6,321 lines under active
//! upstream development. Forking it means inheriting every upstream fix by
//! hand; extracting `buzz-session` now means a large refactor of an
//! actively-developed upstream file in a fork that must keep upmerging, before
//! we know whether the abstraction is right (one consumer cannot tell you).
//! So the policy is reimplemented here — smaller than the refactor, zero
//! upmerge risk, directly testable.
//!
//! **The load-bearing part is the state machine, not the constant table.** The
//! constants below are the cheap half. The expensive half is the recovery
//! behaviour, and §5.2 names five of those as their own T0 cases:
//! `requeue_observer_in_flight` restoring unacked writes **ahead** of newly
//! parked frames (a NOTICE carries no event id, so every unacked frame must be
//! conservatively retried); drop accounting under [`GATED_OBSERVER_QUEUE_CAP`]
//! overflow; the rate-limit gate arm/disarm machine; `n_sub_active` /
//! `observer_control_sub_active` surviving a reconnect; and membership dedup on
//! a strict-`<` watermark.
//!
//! # Time-boxed duplication with a named exit criterion
//!
//! [D-4]: once `buzz-daemon` has run in production through at least one
//! relay-side incident (rate limiting, DNS brownout, or a service restart), the
//! pure-policy half — backoff ladder, `TwoGenDedup`, REQ pacing, the
//! gated-observer queue, `since`-watermark resubscribe — is extracted into a
//! no-I/O `buzz-relay-session` crate and both consumers move onto it, as an
//! *upstream-submittable* PR. Two consumers first, then extract. If that
//! follow-up has not shipped one wave after Wave 1, it becomes a blocker on
//! Wave 3, not a backlog item.
//!
//! Every constant below names `crates/buzz-acp/src/relay.rs` as its origin with
//! the line it was copied from, so the extraction PR can prove equivalence
//! rather than argue it.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The relay's NIP-98 HTTP bridge.
///
/// Re-exported here because `daemon-api.md` §0.3 names the type
/// `session::RestClient`; the implementation lives in [`crate::rest`].
pub use crate::rest::RestClient;

// ── Constant table, copied verbatim per [D-4] ──────────────────────────────
//
// Verified against crates/buzz-acp/src/relay.rs at the cited lines. These are
// copied, NOT re-derived: a re-derived constant that happens to match today
// silently diverges the first time upstream tunes one.

/// Two-generation dedup capacity: each generation holds up to half.
///
/// Origin: `crates/buzz-acp/src/relay.rs:44` (`SEEN_ID_LIMIT`).
pub const SEEN_ID_LIMIT: usize = 12_000;

/// Websocket ping cadence for half-open detection.
///
/// Origin: `crates/buzz-acp/src/relay.rs:47` (`PING_INTERVAL`).
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Pong deadline; exceeding it means the connection is dead even though the
/// socket still looks open.
///
/// Origin: `crates/buzz-acp/src/relay.rs:50` (`PONG_TIMEOUT`).
pub const PONG_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection healthy for this long resets the backoff ladder.
///
/// Origin: `crates/buzz-acp/src/relay.rs:57` (`STABLE_CONNECTION_SECS`).
pub const STABLE_CONNECTION_SECS: u64 = 60;

/// Clock-skew tolerance subtracted from a per-channel `since` watermark on
/// resubscribe (§2.6).
///
/// Origin: `crates/buzz-acp/src/relay.rs:59` (`SINCE_SKEW_SECS`).
pub const SINCE_SKEW_SECS: u64 = 5;

/// Startup/reconnect backoff ladder, shared by initial connect and reconnect.
///
/// Origin: `crates/buzz-acp/src/relay.rs:81` (`STARTUP_CONNECT_BACKOFFS`).
pub const STARTUP_CONNECT_BACKOFFS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// DNS retry interval — deliberately **flat**, not a backoff rung, because a
/// DNS brownout is not congestion (§2.6). Applied with ±20% jitter.
///
/// Origin: `crates/buzz-acp/src/relay.rs:97` (`DNS_RETRY_INTERVAL`).
pub const DNS_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// REQ pacing interval. One REQ per loop tick so a 48-channel resubscribe does
/// not burst past the relay's ~50-frames/5 s admission (§2.6).
///
/// Origin: `crates/buzz-acp/src/relay.rs:102` (`REQ_PACING_INTERVAL`).
pub const REQ_PACING_INTERVAL: Duration = Duration::from_millis(125);

/// Number of parked items drained per loop iteration.
///
/// Origin: `crates/buzz-acp/src/relay.rs:107` (`DRAIN_BUDGET_PER_ITER`).
pub const DRAIN_BUDGET_PER_ITER: usize = 1;

/// Capacity of the gated-observer park queue; overflow is drop-oldest **with
/// visible accounting**, never silent.
///
/// Origin: `crates/buzz-acp/src/relay.rs:112` (`GATED_OBSERVER_QUEUE_CAP`).
pub const GATED_OBSERVER_QUEUE_CAP: usize = 256;

/// Connection state, surfaced **verbatim** to the TUI (§2.6).
///
/// These states are enumerated rather than collapsed because they look
/// identical to "hung" if you collapse them. Auth failure in particular is
/// visually and textually distinct from network failure, with its remediation
/// inline — a NIP-42 rejection or an expired NIP-OA auth tag says so and offers
/// `:login`, it does not say "disconnected" (§1.3 property 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    /// No socket, not trying.
    Disconnected,
    /// TCP/TLS in progress.
    Connecting,
    /// Socket up, NIP-42 challenge in flight.
    Authenticating,
    /// Authenticated and subscribed.
    Connected,
    /// The relay's rate-limit gate is armed; writes return `503` with
    /// `retry_after_ms` and the composer shows a live countdown (§2.7).
    RateLimited {
        /// Milliseconds until the gate is expected to disarm.
        retry_after_ms: u64,
    },
    /// Reconnecting on the backoff ladder.
    Reconnecting {
        /// 1-based ladder position.
        attempt: u32,
        /// Milliseconds until the next attempt, for the status-bar countdown.
        next_retry_in_ms: u64,
    },
    /// DNS is failing. A distinct state because it is not congestion and does
    /// not use the backoff ladder.
    DnsBrownout,
    /// NIP-42 rejected, or the NIP-OA auth tag expired.
    AuthFailed {
        /// Machine-readable cause, e.g. `oa_expired`.
        reason: String,
    },
}

/// Per-channel subscription watermark (§2.6).
///
/// Replayed on reconnect as `since = last_seen - SINCE_SKEW_SECS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinceWatermark {
    /// Newest `created_at` seen on this channel, unix seconds.
    pub last_seen: u64,
}

impl SinceWatermark {
    /// The `since` value to resubscribe with, skew-tolerant and saturating.
    pub fn resubscribe_since(&self) -> u64 {
        self.last_seen.saturating_sub(SINCE_SKEW_SECS)
    }
}

/// Backoff ladder position (§2.6).
///
/// A healthy run of [`STABLE_CONNECTION_SECS`] resets it via [`Self::reset`].
#[derive(Debug, Clone, Copy, Default)]
pub struct BackoffLadder {
    attempt: usize,
}

impl BackoffLadder {
    /// A fresh ladder at rung 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// The delay for the next attempt, clamped at the last rung.
    pub fn next_delay(&mut self) -> Duration {
        let idx = self.attempt.min(STARTUP_CONNECT_BACKOFFS.len() - 1);
        let delay = STARTUP_CONNECT_BACKOFFS[idx];
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// 1-based attempt number for [`ConnectionState::Reconnecting`].
    pub fn attempt(&self) -> u32 {
        self.attempt as u32
    }

    /// Reset after a connection stays up for [`STABLE_CONNECTION_SECS`].
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Two-generation dedup set, ported from `TwoGenDedup`
/// (`crates/buzz-acp/src/relay.rs:948`).
///
/// Rotation, not clearing, is the point. Clearing the whole set at a limit
/// creates an **amnesia window** in which every previously-seen id becomes
/// eligible again; rotating `current` into `previous` at `limit / 2` keeps
/// between `limit / 2` and `limit` ids remembered at all times. The oldest half
/// is forgotten on each rotation — the inherent tradeoff of bounded-memory
/// dedup, acceptable because the `since` watermark is the primary replay
/// protection.
#[derive(Debug)]
pub struct TwoGenDedup {
    current: std::collections::HashSet<String>,
    previous: std::collections::HashSet<String>,
    limit: usize,
}

impl TwoGenDedup {
    /// A dedup set holding between `limit / 2` and `limit` ids.
    pub fn new(limit: usize) -> Self {
        Self {
            current: std::collections::HashSet::new(),
            previous: std::collections::HashSet::new(),
            limit,
        }
    }

    /// Whether `id` is in either generation.
    pub fn contains(&self, id: &str) -> bool {
        self.current.contains(id) || self.previous.contains(id)
    }

    /// Insert `id`; returns `true` when it was new.
    pub fn insert(&mut self, id: String) -> bool {
        if self.contains(&id) {
            return false;
        }
        self.current.insert(id);
        if self.current.len() >= self.limit / 2 {
            self.previous = std::mem::take(&mut self.current);
        }
        true
    }

    /// Forget `id`, so a dropped event can be replayed after reconnect.
    ///
    /// Load-bearing: an event deduped but never delivered to a client would
    /// otherwise be permanently invisible. `relay.rs:2137` removes on exactly
    /// this path.
    pub fn remove(&mut self, id: &str) {
        self.current.remove(id);
        self.previous.remove(id);
    }

    /// How many ids are currently remembered across both generations.
    pub fn len(&self) -> usize {
        self.current.len() + self.previous.len()
    }

    /// Whether nothing has been seen yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drop and queue accounting, served on `GET /daemon` (§2.2, §4.1.4 criterion 5).
///
/// Exit criterion 5 reads these: they must be zero across a normal working day,
/// and any non-zero value must have an explained cause. That is only a usable
/// criterion if every drop path increments one of them.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionCounters {
    /// Observer frames evicted from the park queue under
    /// [`GATED_OBSERVER_QUEUE_CAP`] overflow.
    ///
    /// Origin: `relay.rs`'s `gated_observer_dropped`.
    pub gated_observer_dropped: u64,
    /// Events rejected by [`TwoGenDedup`] — expected to be non-zero after a
    /// reconnect replay, which is why it is reported separately from the drops.
    pub deduped: u64,
    /// Reconnects since the daemon started.
    pub reconnects: u64,
    /// Half-open sockets caught by the ping/pong deadline rather than by a
    /// close frame.
    pub pong_timeouts: u64,
}

/// The gated-observer park queue with its in-flight acknowledgement window.
///
/// [D-4] names this as the expensive half of the port — the constants are
/// cheap, the recovery behaviour is not. Two structures, and the relationship
/// between them is what §5.2's `relay-session recovery` row asserts:
///
/// - `pending` holds frames parked because the rate-limit gate is armed or the
///   socket is down. Bounded at [`GATED_OBSERVER_QUEUE_CAP`], drop-oldest,
///   **counted**.
/// - `in_flight` holds frames written but not yet acknowledged by an `OK`.
///
/// [`Self::requeue_in_flight`] restores `in_flight` to the **front** of
/// `pending`, ahead of frames parked after the gate armed. That ordering is not
/// cosmetic: a `NOTICE` carries no event id, so on a rate-limit notice every
/// unacked frame must be conservatively retried, and retrying them *behind*
/// newer frames would deliver an agent's turn out of order. Duplicate ids are
/// harmless at the relay; out-of-order telemetry is not.
#[derive(Debug, Default)]
pub struct ObserverParkQueue {
    pending: std::collections::VecDeque<String>,
    in_flight: std::collections::VecDeque<String>,
    dropped: u64,
}

impl ObserverParkQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a frame, evicting the oldest **with accounting** on overflow.
    ///
    /// Frames are identified by event id rather than held whole: the daemon
    /// archives the frame itself (`[D-3]`, ciphertext at rest), so the queue
    /// only needs to remember what to re-send.
    pub fn park(&mut self, event_id: String) {
        if self.pending.len() >= GATED_OBSERVER_QUEUE_CAP {
            self.pending.pop_front();
            self.dropped += 1;
        }
        self.pending.push_back(event_id);
    }

    /// Record a frame as written but unacknowledged.
    pub fn track_in_flight(&mut self, event_id: String) {
        if self.in_flight.len() >= GATED_OBSERVER_QUEUE_CAP {
            self.in_flight.pop_front();
            self.dropped += 1;
        }
        self.in_flight.push_back(event_id);
    }

    /// Resolve a frame against a relay `OK`.
    pub fn acknowledge(&mut self, event_id: &str) {
        if let Some(index) = self.in_flight.iter().position(|id| id == event_id) {
            self.in_flight.remove(index);
        }
    }

    /// Restore unacked writes **ahead** of newly parked frames.
    ///
    /// See the type docs for why the ordering is load-bearing. Overflow after
    /// the restore drops from the front — the oldest — and counts it.
    pub fn requeue_in_flight(&mut self) {
        while let Some(event_id) = self.in_flight.pop_back() {
            self.pending.push_front(event_id);
        }
        while self.pending.len() > GATED_OBSERVER_QUEUE_CAP {
            self.pending.pop_front();
            self.dropped += 1;
        }
    }

    /// Drain up to [`DRAIN_BUDGET_PER_ITER`] frames for this loop tick.
    ///
    /// One per tick, paced at [`REQ_PACING_INTERVAL`], is what keeps a
    /// 48-channel resubscribe from bursting past the relay's ~50-frames/5 s
    /// admission window.
    pub fn drain_budget(&mut self) -> Vec<String> {
        (0..DRAIN_BUDGET_PER_ITER)
            .filter_map(|_| self.pending.pop_front())
            .collect()
    }

    /// Frames currently parked.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Frames written but unacknowledged.
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Frames evicted with accounting. Never silent — §4.1.4 criterion 5.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// The subscription registry: what the daemon is subscribed to, and from when.
///
/// Two flags mirror `relay.rs`'s `n_sub_active` / `observer_control_sub_active`
/// and exist for the same reason: a resubscribe after reconnect must restore
/// **which** subscriptions were live, not just the channel set. §5.2 asserts
/// they survive a reconnect.
#[derive(Debug, Default)]
pub struct Subscriptions {
    channels: std::collections::BTreeMap<String, SinceWatermark>,
    /// Whether the membership subscription (44100/44101) was live.
    pub membership_active: bool,
    /// Whether the observer subscription (24200) was live.
    pub observer_active: bool,
    /// Oldest timestamp of a membership event dropped before delivery, so
    /// reconnect replay reaches back far enough to re-deliver it.
    membership_dropped_since: Option<u64>,
    /// Newest membership `created_at` delivered.
    membership_last_seen: Option<u64>,
}

impl Subscriptions {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or refresh) a channel subscription.
    pub fn subscribe(&mut self, channel_id: impl Into<String>) {
        self.channels
            .entry(channel_id.into())
            .or_insert(SinceWatermark { last_seen: 0 });
    }

    /// Drop a channel subscription and its watermark.
    pub fn unsubscribe(&mut self, channel_id: &str) {
        self.channels.remove(channel_id);
    }

    /// Advance a channel's watermark on a delivered event.
    ///
    /// **Strictly greater** wins, which is what makes the watermark monotonic
    /// under out-of-order delivery: a late event with an older `created_at`
    /// must not rewind the replay window and re-deliver everything after it.
    pub fn observe(&mut self, channel_id: &str, created_at: u64) {
        if let Some(mark) = self.channels.get_mut(channel_id) {
            if created_at > mark.last_seen {
                mark.last_seen = created_at;
            }
        }
    }

    /// The `since` value to resubscribe `channel_id` with, skew-tolerant.
    pub fn resubscribe_since(&self, channel_id: &str) -> Option<u64> {
        self.channels
            .get(channel_id)
            .map(SinceWatermark::resubscribe_since)
    }

    /// Every subscribed channel, in a stable order.
    ///
    /// Ordered so a resubscribe burst is reproducible: an unordered map would
    /// make the paced REQ sequence differ run to run, which turns a pacing bug
    /// into an intermittent one.
    pub fn channels(&self) -> impl Iterator<Item = (&String, &SinceWatermark)> {
        self.channels.iter()
    }

    /// How many channels are subscribed. A 48-channel resubscribe at
    /// [`REQ_PACING_INTERVAL`] spreads over ~6 s, which is the point of pacing.
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Whether nothing is subscribed.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Record a membership event that was delivered.
    pub fn observe_membership(&mut self, created_at: u64) {
        self.membership_last_seen = Some(self.membership_last_seen.unwrap_or(0).max(created_at));
    }

    /// Record a membership event that was **dropped before delivery**.
    ///
    /// Tracks the *oldest* such timestamp: replay must reach back to the
    /// earliest thing the client never saw, not the most recent.
    pub fn drop_membership(&mut self, created_at: u64) {
        self.membership_dropped_since = Some(
            self.membership_dropped_since
                .map_or(created_at, |d| d.min(created_at)),
        );
    }

    /// The membership replay watermark, ported from `relay.rs:2578`.
    ///
    /// A dropped event's timestamp wins over the last-seen one — `min`, not
    /// `max` — because the dropped event is *behind* the last delivered one and
    /// replay must start early enough to catch it.
    pub fn membership_replay_since(&self, startup_watermark: Option<u64>) -> Option<u64> {
        match (self.membership_dropped_since, self.membership_last_seen) {
            (Some(d), Some(l)) => Some(d.min(l)),
            (Some(d), None) => Some(d),
            (None, Some(l)) => Some(l),
            (None, None) => startup_watermark,
        }
    }

    /// Clear the dropped-membership marker after a successful resubscribe.
    pub fn clear_membership_dropped(&mut self) {
        self.membership_dropped_since = None;
    }
}

/// Session-layer state over [`buzz_ws_client`].
///
/// Holds the policy half of [D-4]: the connection state, the backoff ladder,
/// the subscription registry with per-channel watermarks, the dedup set, the
/// gated-observer park queue, and the drop accounting. The I/O half — the
/// websocket loop itself — drives this type; keeping the two apart is what
/// makes §5.2's recovery cases testable with no socket, and is the shape the
/// eventual `buzz-relay-session` extraction wants.
#[derive(Debug)]
pub struct Session {
    state: ConnectionState,
    backoff: BackoffLadder,
    /// Subscription registry with per-channel `since` watermarks.
    pub subscriptions: Subscriptions,
    /// Two-generation dedup at [`SEEN_ID_LIMIT`].
    pub seen: TwoGenDedup,
    /// The gated-observer park queue.
    pub observer_queue: ObserverParkQueue,
    counters: SessionCounters,
    /// When the current connection became [`ConnectionState::Connected`], for
    /// the [`STABLE_CONNECTION_SECS`] ladder reset.
    connected_since: Option<std::time::Instant>,
}

impl Session {
    /// A session that has not yet connected.
    pub fn new() -> Self {
        Self {
            state: ConnectionState::Disconnected,
            backoff: BackoffLadder::new(),
            subscriptions: Subscriptions::new(),
            seen: TwoGenDedup::new(SEEN_ID_LIMIT),
            observer_queue: ObserverParkQueue::new(),
            counters: SessionCounters::default(),
            connected_since: None,
        }
    }

    /// Current connection state, as surfaced on `connection.state` (§2.6).
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// Move to a new connection state, maintaining the derived bookkeeping.
    ///
    /// Entering [`ConnectionState::Connected`] starts the stability clock;
    /// leaving it stops it and, when the run was long enough, resets the ladder
    /// — so the next drop after a healthy hour retries at 1 s rather than 16 s.
    pub fn transition(&mut self, next: ConnectionState) {
        let was_connected = matches!(self.state, ConnectionState::Connected);
        let now_connected = matches!(next, ConnectionState::Connected);
        if now_connected && !was_connected {
            self.connected_since = Some(std::time::Instant::now());
        }
        if was_connected && !now_connected {
            if self.was_stable() {
                self.backoff.reset();
            }
            self.connected_since = None;
            self.counters.reconnects += 1;
        }
        self.state = next;
    }

    /// Whether the current connection has been up for
    /// [`STABLE_CONNECTION_SECS`].
    pub fn was_stable(&self) -> bool {
        self.connected_since
            .is_some_and(|since| since.elapsed().as_secs() >= STABLE_CONNECTION_SECS)
    }

    /// Mutable ladder, for the reconnect loop.
    pub fn backoff_mut(&mut self) -> &mut BackoffLadder {
        &mut self.backoff
    }

    /// Whether a fresh event id should be processed, counting duplicates.
    pub fn record_event(&mut self, event_id: &str, channel_id: &str, created_at: u64) -> bool {
        if !self.seen.insert(event_id.to_string()) {
            self.counters.deduped += 1;
            return false;
        }
        self.subscriptions.observe(channel_id, created_at);
        true
    }

    /// Count an event the dedup set rejected.
    ///
    /// Split out from [`Self::record_event`] so a caller can dedupe *before*
    /// routing and advance the watermark *after* it. Doing both in one call
    /// forces the watermark to move for an event the caller has not yet decided
    /// to deliver, and a watermark past an undelivered event is a permanent
    /// loss: the reconnect replay starts after it.
    pub fn note_duplicate(&mut self) {
        self.counters.deduped += 1;
    }

    /// Un-dedup an event that was never delivered, so replay can re-deliver it.
    pub fn forget_event(&mut self, event_id: &str) {
        self.seen.remove(event_id);
    }

    /// Note a pong deadline miss — a half-open socket the OS never reported.
    pub fn record_pong_timeout(&mut self) {
        self.counters.pong_timeouts += 1;
    }

    /// Counters as served on `GET /daemon`, with the queue's own drop total
    /// folded in so there is one place to read (§4.1.4 criterion 5).
    pub fn counters(&self) -> SessionCounters {
        SessionCounters {
            gated_observer_dropped: self.observer_queue.dropped(),
            ..self.counters.clone()
        }
    }

    /// Delay before the next connect attempt, honouring the DNS special case.
    ///
    /// §2.6: a DNS brownout is **not** congestion, so it does not consume a
    /// ladder rung — it retries flat at [`DNS_RETRY_INTERVAL`] with ±20%
    /// jitter. Escalating a name-resolution failure up a congestion ladder
    /// turns a 2-second outage into a 16-second one for no reason.
    pub fn next_retry_delay(&mut self, dns_failure: bool) -> Duration {
        if dns_failure {
            return jittered(DNS_RETRY_INTERVAL);
        }
        self.backoff.next_delay()
    }
}

/// Apply ±20% jitter, so concurrent daemons do not retry on the same tick.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Map [0, 1e9) onto [0.8, 1.2).
    let factor = 0.8 + 0.4 * (f64::from(nanos % 1_000_000) / 1_000_000.0);
    base.mul_f64(factor)
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [D-4] copies the table verbatim. If upstream retunes one of these, this
    /// test is where the divergence shows up as a deliberate decision.
    #[test]
    fn constant_table_matches_relay_rs() {
        assert_eq!(SEEN_ID_LIMIT, 12_000);
        assert_eq!(PING_INTERVAL, Duration::from_secs(30));
        assert_eq!(PONG_TIMEOUT, Duration::from_secs(10));
        assert_eq!(STABLE_CONNECTION_SECS, 60);
        assert_eq!(SINCE_SKEW_SECS, 5);
        assert_eq!(
            STARTUP_CONNECT_BACKOFFS.map(|d| d.as_secs()),
            [1, 2, 4, 8, 16]
        );
        assert_eq!(DNS_RETRY_INTERVAL, Duration::from_secs(2));
        assert_eq!(REQ_PACING_INTERVAL, Duration::from_millis(125));
        assert_eq!(DRAIN_BUDGET_PER_ITER, 1);
        assert_eq!(GATED_OBSERVER_QUEUE_CAP, 256);
    }

    #[test]
    fn ladder_walks_the_rungs_then_clamps() {
        let mut ladder = BackoffLadder::new();
        let seen: Vec<u64> = (0..7).map(|_| ladder.next_delay().as_secs()).collect();
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 16, 16]);
    }

    #[test]
    fn ladder_resets_after_a_stable_run() {
        let mut ladder = BackoffLadder::new();
        ladder.next_delay();
        ladder.next_delay();
        ladder.reset();
        assert_eq!(ladder.next_delay().as_secs(), 1);
        assert_eq!(ladder.attempt(), 1);
    }

    /// §2.6: resubscribe replays with 5 s of skew tolerance, saturating rather
    /// than underflowing for a channel whose only event is near the epoch.
    #[test]
    fn watermark_applies_skew_and_saturates() {
        assert_eq!(
            SinceWatermark { last_seen: 1_000 }.resubscribe_since(),
            1_000 - SINCE_SKEW_SECS
        );
        assert_eq!(SinceWatermark { last_seen: 2 }.resubscribe_since(), 0);
    }

    /// §2.6: the states are surfaced verbatim, so their wire encoding is part
    /// of the contract, not an implementation detail.
    #[test]
    fn connection_states_serialize_with_a_tag() {
        let json = serde_json::to_string(&ConnectionState::Reconnecting {
            attempt: 3,
            next_retry_in_ms: 4_000,
        })
        .unwrap();
        assert!(json.contains("\"state\":\"reconnecting\""), "{json}");
        assert!(json.contains("\"attempt\":3"), "{json}");

        let auth = serde_json::to_string(&ConnectionState::AuthFailed {
            reason: "oa_expired".into(),
        })
        .unwrap();
        assert!(auth.contains("\"state\":\"auth_failed\""), "{auth}");
        assert!(auth.contains("oa_expired"), "{auth}");
    }

    /// §2.6/§1.3 property 3: auth failure is a distinct state, never collapsed
    /// into "disconnected".
    #[test]
    fn auth_failure_is_not_disconnected() {
        let auth = ConnectionState::AuthFailed {
            reason: "nip42_rejected".into(),
        };
        assert_ne!(auth, ConnectionState::Disconnected);
    }

    // ── §5.2 `relay-session recovery` — the five named T0 cases ────────────

    /// Case 1: `requeue_observer_in_flight` restores unacked writes **ahead**
    /// of newly parked frames.
    ///
    /// A NOTICE carries no event id, so on a rate-limit notice every unacked
    /// frame must be conservatively retried — and retrying them behind newer
    /// frames delivers an agent's turn out of order.
    #[test]
    fn requeue_restores_unacked_writes_ahead_of_newly_parked_frames() {
        let mut queue = ObserverParkQueue::new();
        queue.track_in_flight("frame-1".into());
        queue.track_in_flight("frame-2".into());
        // The gate arms; a newer frame parks behind them.
        queue.park("frame-3".into());
        queue.requeue_in_flight();

        assert_eq!(
            queue.drain_budget(),
            vec!["frame-1".to_string()],
            "the oldest unacked write drains first"
        );
        assert_eq!(queue.drain_budget(), vec!["frame-2".to_string()]);
        assert_eq!(
            queue.drain_budget(),
            vec!["frame-3".to_string()],
            "the frame parked after the gate armed drains last"
        );
        assert_eq!(queue.pending_len(), 0);
        assert_eq!(queue.in_flight_len(), 0);
    }

    /// Case 1b: an acknowledged frame is **not** retried — only unacked ones
    /// are, because only those are ambiguous.
    #[test]
    fn an_acknowledged_frame_is_not_requeued() {
        let mut queue = ObserverParkQueue::new();
        queue.track_in_flight("acked".into());
        queue.track_in_flight("unacked".into());
        queue.acknowledge("acked");
        queue.requeue_in_flight();
        assert_eq!(queue.drain_budget(), vec!["unacked".to_string()]);
        assert_eq!(queue.pending_len(), 0);
    }

    /// Case 2: drop accounting under [`GATED_OBSERVER_QUEUE_CAP`] overflow.
    /// §4.1.4 criterion 5 reads this counter, so a silent eviction would make
    /// the criterion unfalsifiable.
    #[test]
    fn park_queue_overflow_drops_oldest_with_visible_accounting() {
        let mut queue = ObserverParkQueue::new();
        for i in 0..GATED_OBSERVER_QUEUE_CAP {
            queue.park(format!("frame-{i}"));
        }
        assert_eq!(queue.dropped(), 0);
        queue.park("one-too-many".into());
        assert_eq!(queue.pending_len(), GATED_OBSERVER_QUEUE_CAP);
        assert_eq!(queue.dropped(), 1);
        assert_eq!(
            queue.drain_budget(),
            vec!["frame-1".to_string()],
            "the oldest frame — frame-0 — is the one that was evicted"
        );
    }

    /// Case 2b: overflow *caused by the requeue itself* is counted too, and it
    /// stays drop-oldest — which after a requeue means the oldest **unacked
    /// write** is what goes, not the newest parked frame.
    ///
    /// Worth pinning because the intuitive reading ("the restored write is the
    /// important one, keep it") is wrong and would invert the eviction order.
    /// The front of `pending` after a requeue is the oldest frame in the whole
    /// system; dropping from anywhere else would deliver telemetry with a hole
    /// in the middle rather than a truncated head. A full pending queue plus a
    /// full in-flight window is exactly the state a sustained rate-limit
    /// produces, so this path runs in practice.
    #[test]
    fn requeue_overflow_drops_the_oldest_and_counts_it() {
        let mut queue = ObserverParkQueue::new();
        for i in 0..GATED_OBSERVER_QUEUE_CAP {
            queue.park(format!("parked-{i}"));
        }
        queue.track_in_flight("oldest-unacked".into());
        queue.requeue_in_flight();
        assert_eq!(queue.pending_len(), GATED_OBSERVER_QUEUE_CAP);
        assert_eq!(queue.dropped(), 1);
        assert_eq!(
            queue.drain_budget(),
            vec!["parked-0".to_string()],
            "the oldest frame overall was evicted, and it was the restored write"
        );
    }

    /// Case 3: the rate-limit gate arm/disarm machine, as seen through the
    /// state enum the TUI renders.
    #[test]
    fn the_rate_limit_gate_is_a_distinct_state_with_a_countdown() {
        let mut session = Session::new();
        session.transition(ConnectionState::Connected);
        session.transition(ConnectionState::RateLimited {
            retry_after_ms: 4_200,
        });
        assert_eq!(
            session.state(),
            &ConnectionState::RateLimited {
                retry_after_ms: 4_200
            }
        );
        session.transition(ConnectionState::Connected);
        assert_eq!(session.state(), &ConnectionState::Connected);
    }

    /// Case 4: `n_sub_active` / `observer_control_sub_active` survive a
    /// reconnect. A resubscribe that restores the channel set but forgets
    /// *which* subscriptions were live leaves the observer feed silently dead.
    #[test]
    fn subscription_flags_survive_a_reconnect() {
        let mut session = Session::new();
        session.subscriptions.subscribe("chan-a");
        session.subscriptions.membership_active = true;
        session.subscriptions.observer_active = true;

        session.transition(ConnectionState::Connected);
        session.transition(ConnectionState::Reconnecting {
            attempt: 1,
            next_retry_in_ms: 1_000,
        });
        session.transition(ConnectionState::Connected);

        assert!(session.subscriptions.membership_active);
        assert!(session.subscriptions.observer_active);
        assert_eq!(session.subscriptions.len(), 1);
    }

    /// Case 5: membership dedup uses a **strict-`<`** watermark — the dropped
    /// timestamp wins over the last-seen one via `min`, because a dropped event
    /// is *behind* the last delivered one and replay must reach back past it.
    #[test]
    fn membership_replay_reaches_back_to_the_oldest_dropped_event() {
        let mut subs = Subscriptions::new();
        subs.observe_membership(1_700_000_100);
        subs.drop_membership(1_700_000_050);
        subs.drop_membership(1_700_000_020);
        assert_eq!(
            subs.membership_replay_since(None),
            Some(1_700_000_020),
            "the oldest dropped timestamp, not the newest delivered one"
        );
        subs.clear_membership_dropped();
        assert_eq!(subs.membership_replay_since(None), Some(1_700_000_100));
    }

    /// With nothing seen and nothing dropped, replay falls back to the startup
    /// watermark rather than to zero — a full-history replay on every restart
    /// would burn the relay budget the pacing exists to protect.
    #[test]
    fn membership_replay_falls_back_to_the_startup_watermark() {
        let subs = Subscriptions::new();
        assert_eq!(
            subs.membership_replay_since(Some(1_699_999_000)),
            Some(1_699_999_000)
        );
        assert_eq!(subs.membership_replay_since(None), None);
    }

    // ── TwoGenDedup ────────────────────────────────────────────────────────

    /// The rotation exists to avoid the amnesia window a full clear creates.
    #[test]
    fn dedup_rotation_does_not_forget_everything_at_once() {
        let mut dedup = TwoGenDedup::new(12);
        for i in 0..6 {
            assert!(dedup.insert(format!("id-{i}")));
        }
        // Rotation happened at len == limit/2; the ids are still remembered.
        for i in 0..6 {
            assert!(dedup.contains(&format!("id-{i}")), "id-{i} was forgotten");
        }
        assert!(
            !dedup.insert("id-0".into()),
            "a rotated id is still a duplicate"
        );
    }

    #[test]
    fn dedup_rejects_duplicates_across_generations() {
        let mut dedup = TwoGenDedup::new(SEEN_ID_LIMIT);
        assert!(dedup.insert("abc".into()));
        assert!(!dedup.insert("abc".into()));
    }

    /// An event deduped but never delivered must be forgettable, or it is
    /// permanently invisible to the client.
    #[test]
    fn a_dropped_event_can_be_forgotten_so_replay_redelivers_it() {
        let mut session = Session::new();
        session.subscriptions.subscribe("chan");
        assert!(session.record_event("evt", "chan", 1_700_000_000));
        assert!(!session.record_event("evt", "chan", 1_700_000_000));
        session.forget_event("evt");
        assert!(session.record_event("evt", "chan", 1_700_000_000));
    }

    // ── Watermarks ─────────────────────────────────────────────────────────

    /// §2.6: the watermark is monotonic. A late event with an older
    /// `created_at` must not rewind the replay window.
    #[test]
    fn a_late_event_does_not_rewind_the_watermark() {
        let mut subs = Subscriptions::new();
        subs.subscribe("chan");
        subs.observe("chan", 1_700_000_100);
        subs.observe("chan", 1_700_000_050);
        assert_eq!(
            subs.resubscribe_since("chan"),
            Some(1_700_000_100 - SINCE_SKEW_SECS)
        );
    }

    /// An unsubscribed channel has no watermark, so a resubscribe cannot
    /// silently use a stale one.
    #[test]
    fn unsubscribing_drops_the_watermark() {
        let mut subs = Subscriptions::new();
        subs.subscribe("chan");
        subs.observe("chan", 1_700_000_100);
        subs.unsubscribe("chan");
        assert_eq!(subs.resubscribe_since("chan"), None);
        assert!(subs.is_empty());
    }

    /// The resubscribe order must be stable, or a pacing bug becomes
    /// intermittent rather than reproducible.
    #[test]
    fn resubscribe_order_is_deterministic() {
        let mut subs = Subscriptions::new();
        for id in ["c", "a", "b"] {
            subs.subscribe(id);
        }
        let order: Vec<&str> = subs.channels().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, ["a", "b", "c"]);
    }

    // ── Backoff and the DNS special case ──────────────────────────────────

    /// §2.6: a DNS brownout does **not** consume a ladder rung. Escalating a
    /// name-resolution failure up a congestion ladder turns a 2 s outage into a
    /// 16 s one for no reason.
    #[test]
    fn a_dns_brownout_does_not_consume_a_ladder_rung() {
        let mut session = Session::new();
        for _ in 0..5 {
            let delay = session.next_retry_delay(true);
            // ±20% jitter around 2 s.
            assert!(
                delay >= DNS_RETRY_INTERVAL.mul_f64(0.8)
                    && delay <= DNS_RETRY_INTERVAL.mul_f64(1.2),
                "{delay:?}"
            );
        }
        // The ladder is untouched: the next real failure still starts at 1 s.
        assert_eq!(session.next_retry_delay(false).as_secs(), 1);
    }

    #[test]
    fn a_congestion_failure_walks_the_ladder() {
        let mut session = Session::new();
        let seen: Vec<u64> = (0..3)
            .map(|_| session.next_retry_delay(false).as_secs())
            .collect();
        assert_eq!(seen, vec![1, 2, 4]);
    }

    /// A reconnect after an *unstable* run keeps climbing — resetting there
    /// would hammer a relay that is flapping.
    #[test]
    fn an_unstable_run_does_not_reset_the_ladder() {
        let mut session = Session::new();
        session.next_retry_delay(false);
        session.next_retry_delay(false);
        session.transition(ConnectionState::Connected);
        session.transition(ConnectionState::Disconnected);
        assert_eq!(
            session.next_retry_delay(false).as_secs(),
            4,
            "the ladder continued from rung 2 rather than resetting to 1 s"
        );
    }

    /// §4.1.4 criterion 5: every drop path moves a counter, and the queue's
    /// total is folded into the one `GET /daemon` reads.
    #[test]
    fn counters_surface_every_drop_path() {
        let mut session = Session::new();
        session.subscriptions.subscribe("chan");
        session.record_event("evt", "chan", 1);
        session.record_event("evt", "chan", 1);
        session.record_pong_timeout();
        for i in 0..=GATED_OBSERVER_QUEUE_CAP {
            session.observer_queue.park(format!("f-{i}"));
        }
        session.transition(ConnectionState::Connected);
        session.transition(ConnectionState::Disconnected);

        let counters = session.counters();
        assert_eq!(counters.deduped, 1);
        assert_eq!(counters.pong_timeouts, 1);
        assert_eq!(counters.gated_observer_dropped, 1);
        assert_eq!(counters.reconnects, 1);
    }
}
