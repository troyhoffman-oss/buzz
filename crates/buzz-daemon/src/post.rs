//! Posting — visible pending, never a silent queue.
//!
//! Implements the send half of `DESIGN.md` §2.7 and §2.4 [D-2]/[D-7], and the
//! `POST /channel/{id}/message` and `POST /message/{id}/ask` endpoints of the
//! Wave-1 subset.
//!
//! # There is no write queue, and there is no silent loss
//!
//! §1.4 makes "no offline write queue" a non-goal, and §2.7 is what makes that
//! honest rather than lossy:
//!
//! - A send while the relay is down returns `503 relay_unreachable`. The TUI
//!   **keeps the composed text in the composer**, marks it pending, and binds an
//!   explicit retry. The daemon does not hold it.
//! - A send while the rate-limit gate is armed returns `503` with
//!   `retry_after_ms`, so the composer shows a live countdown rather than a
//!   spinner.
//!
//! The distinction that makes this design work: a *visible* pending message the
//! operator can see and retry is honest; a queue that drains later is a message
//! that arrives at a time nobody chose, into a conversation that has moved on.
//!
//! # [D-7] `local_id` is a daemon-side map only
//!
//! The daemon computes the event id at sign time, so correlating the relay `OK`
//! back to the client's provisional id needs **no wire change and no tag on the
//! event**. Stated here as well as in the design so nobody adds one: a
//! `local_id` tag would leak a client's UI bookkeeping into permanent relay
//! history.

use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};
use crate::identity::Identity;
use crate::mentions::{check_mention_cap, MENTION_CAP};
use crate::session::ConnectionState;

/// A send request from a client.
#[derive(Debug, Clone, Deserialize)]
pub struct SendRequest {
    /// Message body.
    pub content: String,
    /// Root event id when this is a threaded reply.
    pub reply_to: Option<String>,
    /// **Resolved pubkeys**, per [D-2]. The TUI always sends these; the
    /// server-side name resolution path exists for `curl` and second clients.
    #[serde(default)]
    pub mentions: Vec<String>,
    /// Whether a threaded reply also reaches the channel window.
    #[serde(default)]
    pub broadcast: bool,
    /// Client-side provisional id, echoed back for optimistic reconcile.
    ///
    /// [D-7]: a daemon-side map only. It never touches the event.
    pub local_id: Option<String>,
}

/// The response to a send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResponse {
    /// The signed event's id.
    pub event_id: String,
    /// Whether the relay accepted it.
    pub accepted: bool,
    /// The relay's message, when it had one.
    pub message: String,
    /// The client's provisional id, echoed so the optimistic bubble can be
    /// reconciled without a content match — content matching breaks the moment
    /// two identical messages are sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_id: Option<String>,
}

/// Reject a send that cannot succeed, **before** it is signed (§2.7).
///
/// Order matters. The cap is checked first because it is a property of the
/// message the operator has already written: telling them the relay is
/// unreachable, waiting for it to come back, and *then* telling them there are
/// too many mentions is two round trips of bad news for one message.
pub fn check_sendable(request: &SendRequest, state: &ConnectionState) -> Result<()> {
    // §2.4: `MENTION_CAP` is a **build-time** rejection in the SDK, so it must
    // be surfaced before the send, not after — failing at Enter on a message
    // already written is the worst possible place to learn about a cap.
    check_mention_cap(request.mentions.len()).map_err(|e| DaemonError::TooManyMentions {
        cap: e.cap,
        requested: e.requested,
    })?;

    match state {
        ConnectionState::Connected => Ok(()),
        ConnectionState::RateLimited { retry_after_ms } => Err(DaemonError::RateLimited {
            retry_after_ms: *retry_after_ms,
        }),
        ConnectionState::AuthFailed { .. } => Err(DaemonError::NotAuthenticated),
        // Every other state is "the relay is not there right now". They are
        // distinct on the *status* surface (§2.6 renders each one differently),
        // but for a write they are one outcome: it did not go, keep the text.
        _ => Err(DaemonError::RelayUnreachable),
    }
}

/// Build and sign a channel message (§4.1 scope: channels/threads/posting).
///
/// Delegates the event construction to `buzz_sdk::build_message` — the SDK is
/// the operation vocabulary and reimplementing a builder here would fork the
/// validation that goes with it. Signing goes through
/// [`Identity::sign_event`], which asserts exactly one NIP-OA auth tag.
pub fn build_message_event(
    identity: &Identity,
    channel_id: &str,
    request: &SendRequest,
) -> Result<nostr::Event> {
    let uuid = uuid::Uuid::parse_str(channel_id).map_err(|_| {
        DaemonError::InvalidInput(format!("channel id is not a uuid: {channel_id}"))
    })?;
    let mentions: Vec<&str> = request.mentions.iter().map(String::as_str).collect();
    let thread_ref = match request.reply_to.as_deref() {
        Some(root) => {
            // Parsed rather than passed through: an unparseable root would
            // produce an `e` tag the relay stores and no client can resolve —
            // a reply that exists under nothing.
            let root = nostr::EventId::from_hex(root).map_err(|_| {
                DaemonError::InvalidInput(format!("reply_to is not an event id: {root}"))
            })?;
            Some(buzz_sdk::ThreadRef {
                root_event_id: root,
                parent_event_id: root,
            })
        }
        None => None,
    };

    let builder = buzz_sdk::build_message(
        uuid,
        &request.content,
        thread_ref.as_ref(),
        &mentions,
        request.broadcast,
        &[],
    )
    .map_err(|e| DaemonError::Sdk(e.to_string()))?;
    identity.sign_event(builder)
}

/// Build and sign an ask-card answer (§2.4, §3.4.1).
///
/// A **threaded kind:9 reply** carrying the `broadcast` tag. Not a control
/// frame: `POST /agent/{pk}/control` accepts exactly two payloads, and routing
/// an answer there would be logged-and-dropped by the harness *silently*.
pub fn build_ask_answer_event(
    identity: &Identity,
    channel_id: &str,
    root_event_id: &str,
    agent_pubkey: &str,
    indices: &[usize],
) -> Result<nostr::Event> {
    let request = SendRequest {
        content: crate::askcard::ask_reply_content(indices),
        reply_to: Some(root_event_id.to_string()),
        mentions: crate::askcard::ask_reply_mentions(agent_pubkey),
        // **Always** broadcast: §2.4 is explicit that a thread-only reply never
        // reaches the channel window and the card's answered-state derivation
        // breaks without it. Not a caller's choice.
        broadcast: true,
        local_id: None,
    };
    build_message_event(identity, channel_id, &request)
}

/// Correlate a relay `OK` back to a client's provisional id ([D-7]).
///
/// A daemon-side map keyed by the event id the daemon computed at sign time.
/// Bounded, because a client that sends and never reads its own stream would
/// otherwise grow this without limit.
#[derive(Debug, Default)]
pub struct LocalIdMap {
    entries: std::collections::VecDeque<(String, String)>,
}

impl LocalIdMap {
    /// Capacity of the correlation window.
    ///
    /// Generous relative to any plausible in-flight send count, small enough
    /// that a misbehaving client cannot grow it into a leak.
    pub const CAPACITY: usize = 256;

    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the correlation for a signed event.
    pub fn record(&mut self, event_id: impl Into<String>, local_id: impl Into<String>) {
        if self.entries.len() >= Self::CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((event_id.into(), local_id.into()));
    }

    /// Take the provisional id for an event id, if one was recorded.
    ///
    /// Taking rather than reading: a `local_id` is consumed by the reconcile it
    /// was recorded for, and a second delivery of the same event (a reconnect
    /// replay) must not re-fire the optimistic reconcile a second time.
    pub fn take(&mut self, event_id: &str) -> Option<String> {
        let index = self.entries.iter().position(|(id, _)| id == event_id)?;
        self.entries.remove(index).map(|(_, local)| local)
    }

    /// How many correlations are outstanding.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is outstanding.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Whether a mention list is within the SDK's build-time cap.
pub fn mentions_within_cap(count: usize) -> bool {
    count <= MENTION_CAP
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    const CHANNEL: &str = "11111111-1111-1111-1111-111111111111";

    fn identity() -> Identity {
        Identity::from_keys(nostr::Keys::generate(), None)
    }

    fn request(content: &str) -> SendRequest {
        SendRequest {
            content: content.into(),
            reply_to: None,
            mentions: Vec::new(),
            broadcast: false,
            local_id: Some("pending-1".into()),
        }
    }

    // ── §2.7 — visible pending, never a silent queue ──────────────────────

    #[test]
    fn a_send_on_a_healthy_connection_is_allowed() {
        check_sendable(&request("hello"), &ConnectionState::Connected).unwrap();
    }

    /// §2.7: "a send while link B is down returns `503 relay_unreachable`. The
    /// TUI **keeps the composed text in the composer**." The daemon does not
    /// hold it — that would be the write queue §1.4 rules out.
    #[test]
    fn a_send_while_disconnected_is_refused_not_queued() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Authenticating,
            ConnectionState::DnsBrownout,
            ConnectionState::Reconnecting {
                attempt: 2,
                next_retry_in_ms: 4_000,
            },
        ] {
            let err = check_sendable(&request("hello"), &state).unwrap_err();
            assert_eq!(err.code(), "relay_unreachable", "{state:?}");
            assert_eq!(err.status(), 503);
        }
    }

    /// §2.7: "a send while `rate_limited` is armed returns `503` with
    /// `retry_after_ms`, and the composer shows a live countdown rather than a
    /// spinner." The countdown is only possible because the number is on the
    /// error.
    #[test]
    fn a_rate_limited_send_carries_its_countdown() {
        let err = check_sendable(
            &request("hello"),
            &ConnectionState::RateLimited {
                retry_after_ms: 4_200,
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "rate_limited");
        assert_eq!(err.retry_after_ms(), Some(4_200));
        let body = crate::error::ErrorBody::from(&err);
        assert_eq!(body.retry_after_ms, Some(4_200));
    }

    /// §2.6/§1.3 property 3: auth failure is distinct from network failure, and
    /// stays distinct on the write path — the remedy is `:login`, not "retry".
    #[test]
    fn an_auth_failure_is_not_reported_as_unreachable() {
        let err = check_sendable(
            &request("hello"),
            &ConnectionState::AuthFailed {
                reason: "oa_expired".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "not_authenticated");
        assert_eq!(err.status(), 401);
    }

    /// §2.4: the cap is surfaced **before** the send, and before the connection
    /// check — telling the operator the relay is down, then telling them there
    /// are too many mentions, is two round trips of bad news for one message.
    #[test]
    fn the_mention_cap_is_reported_even_while_disconnected() {
        let mut req = request("hello");
        req.mentions = (0..MENTION_CAP + 1).map(|n| format!("{n:064}")).collect();
        let err = check_sendable(&req, &ConnectionState::Disconnected).unwrap_err();
        assert_eq!(err.code(), "too_many_mentions");
        assert_eq!(err.status(), 400);

        let detail = crate::error::ErrorBody::from(&err).detail.unwrap();
        assert_eq!(detail["cap"], serde_json::json!(MENTION_CAP));
        assert_eq!(detail["requested"], serde_json::json!(MENTION_CAP + 1));
    }

    #[test]
    fn exactly_the_cap_is_allowed() {
        let mut req = request("hello");
        req.mentions = (0..MENTION_CAP).map(|n| format!("{n:064}")).collect();
        check_sendable(&req, &ConnectionState::Connected).unwrap();
        assert!(mentions_within_cap(MENTION_CAP));
        assert!(!mentions_within_cap(MENTION_CAP + 1));
    }

    // ── Event construction ────────────────────────────────────────────────

    #[test]
    fn a_message_carries_its_channel_scope() {
        let identity = identity();
        let event = build_message_event(&identity, CHANNEL, &request("hello")).unwrap();
        assert_eq!(event.kind.as_u16(), 9);
        assert_eq!(event.content, "hello");
        let h = event
            .tags
            .iter()
            .map(nostr::Tag::as_slice)
            .find(|t| t.first().map(String::as_str) == Some("h"))
            .expect("channels are scoped by `h`, not `e`");
        assert_eq!(h[1], CHANNEL);
    }

    /// [D-7]: `local_id` is a daemon-side map only. A tag would leak a client's
    /// UI bookkeeping into permanent relay history.
    #[test]
    fn the_local_id_never_reaches_the_event() {
        let identity = identity();
        let event = build_message_event(&identity, CHANNEL, &request("hello")).unwrap();
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("pending-1"), "{json}");
        assert!(!json.contains("local_id"), "{json}");
    }

    /// A threaded reply carries the NIP-10 root marker, so it lands under its
    /// question rather than at the channel root.
    #[test]
    fn a_reply_carries_its_thread_reference() {
        let identity = identity();
        let root = "ab".repeat(32);
        let mut req = request("answer");
        req.reply_to = Some(root.clone());
        let event = build_message_event(&identity, CHANNEL, &req).unwrap();
        let e_tag = event
            .tags
            .iter()
            .map(nostr::Tag::as_slice)
            .find(|t| t.first().map(String::as_str) == Some("e"))
            .expect("a reply references its root");
        assert_eq!(e_tag[1], root);
    }

    #[test]
    fn a_malformed_channel_id_is_an_input_error_not_a_panic() {
        let identity = identity();
        let err = build_message_event(&identity, "not-a-uuid", &request("x")).unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert_eq!(err.status(), 400);
    }

    /// A keyless daemon cannot sign, and says so rather than producing an
    /// unsigned event.
    #[test]
    fn a_keyless_daemon_cannot_sign_a_message() {
        let keyless = Identity::new("aa".repeat(32), Zeroizing::new(Vec::new()), None);
        let err = build_message_event(&keyless, CHANNEL, &request("x")).unwrap_err();
        assert_eq!(err.code(), "identity_decrypt_failed");
    }

    // ── The ask answer ────────────────────────────────────────────────────

    /// §2.4: the answer **always** broadcasts — a thread-only reply never
    /// reaches the channel window and the card's answered state never derives.
    /// Not a caller's choice.
    #[test]
    fn an_ask_answer_always_broadcasts_and_p_tags_the_agent() {
        let identity = identity();
        let agent = "cd".repeat(32);
        let event =
            build_ask_answer_event(&identity, CHANNEL, &"ab".repeat(32), &agent, &[0, 2]).unwrap();

        let has = |name: &str| {
            event
                .tags
                .iter()
                .map(nostr::Tag::as_slice)
                .any(|t| t.first().map(String::as_str) == Some(name))
        };
        assert!(has("broadcast"), "without it the card never resolves");
        assert!(has("e"), "the answer threads under the question");

        let p = event
            .tags
            .iter()
            .map(nostr::Tag::as_slice)
            .find(|t| t.first().map(String::as_str) == Some("p"))
            .expect("without a p tag the agent never receives the answer");
        assert_eq!(p[1], agent);

        // 1-based indices, per `askReplyContent`.
        assert_eq!(event.content, "1, 3");
    }

    // ── [D-7] the correlation map ─────────────────────────────────────────

    #[test]
    fn a_local_id_correlates_back_from_the_event_id() {
        let mut map = LocalIdMap::new();
        map.record("event-1", "pending-1");
        map.record("event-2", "pending-2");
        assert_eq!(map.take("event-2").as_deref(), Some("pending-2"));
        assert_eq!(map.take("event-1").as_deref(), Some("pending-1"));
        assert!(map.is_empty());
    }

    /// Taking rather than reading: a second delivery of the same event — a
    /// reconnect replay — must not re-fire the optimistic reconcile.
    #[test]
    fn a_correlation_is_consumed_by_its_reconcile() {
        let mut map = LocalIdMap::new();
        map.record("event-1", "pending-1");
        assert!(map.take("event-1").is_some());
        assert!(
            map.take("event-1").is_none(),
            "a replayed event must not reconcile twice"
        );
    }

    /// Bounded, so a client that sends and never reads its own stream cannot
    /// grow this into a leak.
    #[test]
    fn the_correlation_window_is_bounded() {
        let mut map = LocalIdMap::new();
        for n in 0..=LocalIdMap::CAPACITY {
            map.record(format!("event-{n}"), format!("pending-{n}"));
        }
        assert_eq!(map.len(), LocalIdMap::CAPACITY);
        assert!(
            map.take("event-0").is_none(),
            "the oldest correlation was evicted"
        );
        assert!(map
            .take(&format!("event-{}", LocalIdMap::CAPACITY))
            .is_some());
    }

    /// An unknown event id is simply not ours — a message from another client
    /// on the same identity, which is the second-client case §1.5 calls real.
    #[test]
    fn an_unknown_event_has_no_correlation() {
        let mut map = LocalIdMap::new();
        assert!(map.take("someone-elses-event").is_none());
    }
}
