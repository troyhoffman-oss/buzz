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

/// Session-layer handle over [`buzz_ws_client`].
///
/// TODO(wave1, §4.1.1 deliverable 1): implement the state machine. The
/// constants above are the cheap half; §5.2's `relay-session recovery` row is
/// the half that must be tested. Ordered work:
/// 1. NIP-42 connect/auth via `buzz_ws_client::connect_authenticated`, carrying
///    the NIP-OA auth tag into the kind-22242 event (§2.5).
/// 2. Subscription registry keyed by channel, holding [`SinceWatermark`].
/// 3. `TwoGenDedup` at [`SEEN_ID_LIMIT`] with the **strict-`<`** membership
///    watermark (§5.2).
/// 4. Ping/pong at [`PING_INTERVAL`]/[`PONG_TIMEOUT`].
/// 5. [`BackoffLadder`] + the flat [`DNS_RETRY_INTERVAL`] branch.
/// 6. REQ pacing at [`REQ_PACING_INTERVAL`], [`DRAIN_BUDGET_PER_ITER`] per tick,
///    with a shutdown-aware `pacing_sleep` that defers commands.
/// 7. The gated-observer park queue at [`GATED_OBSERVER_QUEUE_CAP`] with
///    `requeue_observer_in_flight` restoring unacked writes **ahead** of newly
///    parked frames, and `gated_observer_dropped` accounting exposed on
///    `GET /daemon`.
#[derive(Debug)]
pub struct Session {
    state: ConnectionState,
    backoff: BackoffLadder,
}

impl Session {
    /// A session that has not yet connected.
    pub fn new() -> Self {
        Self {
            state: ConnectionState::Disconnected,
            backoff: BackoffLadder::new(),
        }
    }

    /// Current connection state, as surfaced on `connection.state` (§2.6).
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// Mutable ladder, for the reconnect loop.
    pub fn backoff_mut(&mut self) -> &mut BackoffLadder {
        &mut self.backoff
    }
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
}
