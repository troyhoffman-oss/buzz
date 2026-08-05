//! The relay I/O loop — the half [`crate::session`] describes and does not drive.
//!
//! Implements `DESIGN.md` §2.6 (reconnect), §2.7 (offline behaviour), and the
//! I/O half of Wave-1 daemon deliverable 1 (§4.1.1). `session.rs` is the policy:
//! the connection-state machine, the subscription registry with its per-channel
//! `since` watermarks, the dedup set, the park queue, the backoff ladder. This
//! module is the websocket task that turns that policy into traffic, plus the
//! ingest function that turns relay frames into store writes and `/event`
//! frames.
//!
//! # Shape: a sequential loop, not a `select!`
//!
//! [`buzz_ws_client::NostrWsConnection::next_event`] is **not cancel-safe** —
//! `recv_one` (`crates/buzz-ws-client/src/connection.rs:134`) polls `ws.next()`
//! inside a `timeout`, so dropping that future mid-poll, which is exactly what
//! a losing `select!` branch does, can lose a partially-read frame. So the loop
//! reads with a short timeout and does everything else — draining commands,
//! pacing REQs, firing timers — *between* reads. No future is ever cancelled.
//!
//! This is also the shape [`crate::session::DRAIN_BUDGET_PER_ITER`] already
//! describes: one paced action per loop tick, one tick per
//! [`crate::session::REQ_PACING_INTERVAL`].
//!
//! # Liveness is a REQ/EOSE probe, not a websocket ping
//!
//! §2.6 asks for 30 s ping / 10 s pong. `buzz-ws-client` cannot express it: an
//! inbound `Ping` is answered internally and a `Pong` is swallowed by the
//! `_ => {}` arm (`connection.rs:148`–`152`), so a pong is not observable
//! through the public API, and adding a method forks the crate §2.1 says is
//! used *verbatim*.
//!
//! The probe is instead a NIP-01 `REQ` with a filter that cannot match. Every
//! relay answers a `REQ` with `EOSE` — it is core NIP-01 — it returns no rows,
//! and its explicit `kinds` satisfies §2.4's global invariant by construction.
//! Cadence and deadline stay [`crate::session::PING_INTERVAL`] /
//! [`crate::session::PONG_TIMEOUT`], and **any** inbound frame counts as
//! liveness evidence, so on a busy relay no probe is ever sent.
//!
//! # Writes go over the HTTP bridge, not the socket
//!
//! [`crate::rest::RestClient::submit_event`] already carries §2.7's whole retry
//! policy: NIP-98 re-signed per attempt, moderation kinds never blindly
//! retried, the `x-auth-tag` header. Publishing over the websocket would fork
//! all three onto a second path. The socket is the *read* channel; the bridge
//! is the *write* channel. `connection.state` still gates writes
//! ([`crate::post::check_sendable`]) because what it reports is whether the
//! relay is reachable at all.
//!
//! # In-flight publishes survive exactly one reconnect, then fail visibly
//!
//! §2.7 forbids a write queue and forbids silent loss. A publish whose outcome
//! never arrived is *ambiguous* — the relay may have stored it — and each of
//! the three available answers is wrong except one:
//!
//! - Fail immediately → the operator retries → a second `created_at`, a second
//!   event id, a duplicate message. Retry-by-recompose cannot be idempotent.
//! - Queue indefinitely → the message lands at a time nobody chose, which is
//!   the exact failure the no-write-queue rule exists to prevent.
//! - **Re-publish the same signed event** → identical event id, so relay dedup
//!   makes it idempotent, bounded by [`PUBLISH_DEADLINE`]. Past that the caller
//!   gets `503 relay_unreachable`, the TUI keeps the composed text, and the
//!   retry is the operator's explicit choice.
//!
//! # Read-only is structural, not a flag
//!
//! The loop publishes on exactly two paths: an explicit
//! [`WireCommand::Publish`], and the read-state debounce — which fires only
//! when [`crate::readstate::ReadState::is_dirty`], which only a client `mark`
//! sets. A caller that issues no command and marks nothing **cannot** write.
//! `tests/live_relay.rs` asserts that mechanically rather than trusting it.

use std::time::{Duration, Instant};

use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};

use crate::error::{DaemonError, Result};
use crate::session::ConnectionState;
use crate::state::{AppState, Inner};

/// Read timeout for one loop tick.
///
/// Deliberately short and unrelated to any protocol deadline: it is the loop's
/// *scheduling* granularity, not a timeout on anything. A tick that expires
/// with no frame is the loop's opportunity to drain a command, send one paced
/// REQ, and check its timers. Matching [`crate::session::REQ_PACING_INTERVAL`]
/// means a resubscribe walks at exactly the paced rate with no extra timer.
pub const TICK: Duration = Duration::from_millis(125);

/// How long an unresolved publish is carried before it fails visibly (§2.7).
pub const PUBLISH_DEADLINE: Duration = Duration::from_secs(30);

/// Live-tail page size for a channel subscription.
///
/// A subscription is a *tail*, not a history read: the paged history walk is
/// [`crate::timeline::build_window_filter`] over the HTTP bridge. This bounds
/// the catch-up burst a reconnect replays before the tail goes live.
pub const CHANNEL_TAIL_LIMIT: u32 = 200;

/// Subscription id for the channel timeline of `channel_id`.
///
/// Ingest dispatches on the **subscription**, not on the event kind: kind alone
/// is ambiguous — a kind-9 arrives on both a channel tail and a mention inbox —
/// and re-deriving intent from the payload is how the two get confused.
pub fn channel_sub_id(channel_id: &str) -> String {
    format!("{SUB_CHANNEL_PREFIX}{channel_id}")
}

/// Prefix of a channel subscription id.
pub const SUB_CHANNEL_PREFIX: &str = "ch:";
/// Subscription id for membership notifications (44100/44101).
pub const SUB_MEMBERSHIP: &str = "member";
/// Subscription id for observer frames (24200).
pub const SUB_OBSERVER: &str = "observer";
/// Subscription id for turn metrics (44200).
pub const SUB_METRIC: &str = "metric";
/// Subscription id for live presence beats (20001).
pub const SUB_PRESENCE: &str = "presence";
/// Subscription id for the liveness probe.
pub const SUB_PROBE: &str = "probe";

/// A command from the HTTP surface to the relay loop.
///
/// The loop owns the socket, so every write crosses this channel. Each variant
/// carries its own responder rather than sharing one: a caller waiting on a
/// publish needs the relay's verdict, and a caller asking for a reconnect needs
/// only that it happened.
#[derive(Debug)]
pub enum WireCommand {
    /// Publish a signed event and report the outcome.
    Publish {
        /// The already-signed event. Signing happens on the request path so a
        /// build error reaches the client as `400` rather than as a timeout.
        event: Box<nostr::Event>,
        /// Where the outcome goes. `None` is fire-and-forget (typing).
        respond: Option<tokio::sync::oneshot::Sender<Result<crate::post::SendResponse>>>,
        /// [D-7] provisional id, echoed on the response and the stream frame.
        local_id: Option<String>,
    },
    /// Open a live subscription on a channel's timeline.
    Subscribe {
        /// Channel uuid.
        channel_id: String,
    },
    /// Force a reconnect — `POST /daemon/reconnect`.
    Reconnect,
}

/// The handle the HTTP surface holds to talk to the relay loop.
///
/// Cloneable and `Send`, so every route handler can hold one. A closed channel
/// means the loop is gone, which is reported as
/// [`DaemonError::RelayUnreachable`] rather than as a panic: a daemon whose
/// relay task died must still answer reads from cache (§2.7).
#[derive(Debug, Clone)]
pub struct WireHandle {
    tx: tokio::sync::mpsc::Sender<WireCommand>,
}

impl WireHandle {
    /// Send a command, or report the loop as unreachable.
    pub async fn send(&self, command: WireCommand) -> Result<()> {
        self.tx
            .send(command)
            .await
            .map_err(|_| DaemonError::RelayUnreachable)
    }

    /// Publish a signed event and wait for the relay's verdict.
    ///
    /// The deadline is the loop's, not this caller's: the loop holds the event
    /// across at most one reconnect ([`PUBLISH_DEADLINE`]) and then answers. A
    /// caller-side timeout shorter than that would report failure for a publish
    /// still in flight — precisely the ambiguous outcome §2.7 exists to avoid.
    pub async fn publish(
        &self,
        event: nostr::Event,
        local_id: Option<String>,
    ) -> Result<crate::post::SendResponse> {
        let (respond, wait) = tokio::sync::oneshot::channel();
        self.send(WireCommand::Publish {
            event: Box::new(event),
            respond: Some(respond),
            local_id,
        })
        .await?;
        wait.await.map_err(|_| DaemonError::RelayUnreachable)?
    }

    /// Publish without waiting — the ephemeral path (typing).
    ///
    /// `daemon-api.md` §3.5: typing is dropped, not queued, when the gate is
    /// armed, and `POST /channel/{id}/typing` returns `202` always. Waiting on
    /// an acknowledgement for a frame that is allowed to be dropped would turn
    /// a fire-and-forget into a request that can fail.
    pub async fn publish_detached(&self, event: nostr::Event) -> Result<()> {
        self.send(WireCommand::Publish {
            event: Box::new(event),
            respond: None,
            local_id: None,
        })
        .await
    }
}

/// Build a command channel and its handle.
///
/// The buffer is small on purpose: back-pressure on the command channel is the
/// honest signal that the loop is not draining, and a large buffer converts it
/// into latency the caller cannot see.
pub fn channel() -> (WireHandle, tokio::sync::mpsc::Receiver<WireCommand>) {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    (WireHandle { tx }, rx)
}

/// What [`apply_relay_event`] did with one event.
///
/// Returned rather than logged so the loop's routing is testable with no
/// socket, and so a quiet feed can be attributed to a specific cause
/// (§4.1.4 criterion 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingested {
    /// Applied to a store; the named stream topics were published.
    Applied(Vec<String>),
    /// Rejected by the dedup set — expected after a reconnect replay, which is
    /// why it is distinct from a drop.
    Duplicate,
    /// Dropped, with the reason. Never silent.
    Dropped(&'static str),
}

impl Ingested {
    /// The stream topics this ingest published, if any.
    pub fn topics(&self) -> &[String] {
        match self {
            Self::Applied(topics) => topics,
            _ => &[],
        }
    }
}

/// Read an event's kind as the `u32` the stores speak.
fn kind_of(event: &nostr::Event) -> u32 {
    u32::from(event.kind.as_u16())
}

/// First value of a tag, by name.
fn tag_value(event: &nostr::Event, name: &str) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let slice = tag.as_slice();
        if slice.first().map(String::as_str) == Some(name) {
            slice.get(1).cloned()
        } else {
            None
        }
    })
}

/// Whether the event `p`-tags `pubkey`.
fn tags_pubkey(event: &nostr::Event, pubkey: &str) -> bool {
    event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.first().map(String::as_str) == Some("p")
            && slice.get(1).map(String::as_str) == Some(pubkey)
    })
}

/// The channel uuid an event is scoped to, from its NIP-29 `h` tag.
fn channel_of(event: &nostr::Event) -> Option<String> {
    tag_value(event, "h")
}

/// Every tag row as a `Vec<String>` — the shape [`crate::askcard`] parses.
fn tag_rows(event: &nostr::Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

/// The event as the JSON the stores and the stream speak.
///
/// The stores take `serde_json::Value` because they were built against the HTTP
/// bridge, which returns JSON. Converting once here rather than teaching every
/// store a second input type keeps one representation in the cache regardless
/// of which transport delivered the event.
fn event_json(event: &nostr::Event) -> serde_json::Value {
    serde_json::to_value(event).unwrap_or(serde_json::Value::Null)
}

/// Apply one relay event to the stores and publish the frames it implies.
///
/// A pure function over [`Inner`]: it does the store work and the stream
/// publishing, and the I/O loop only decides *when* to call it. That is the
/// same policy/transport split [`crate::session`] already uses, and it is what
/// makes routing testable with no socket.
///
/// `subscription_id` decides the route, not the kind — see [`channel_sub_id`].
pub fn apply_relay_event(
    inner: &mut Inner,
    subscription_id: &str,
    event: &nostr::Event,
    now: i64,
) -> Ingested {
    let kind = kind_of(event);
    let event_id = event.id.to_hex();
    let created_at = event.created_at.as_secs();
    // The watermark key is the `h` tag when there is one and the subscription
    // otherwise: a watermark is per-channel for channel traffic and
    // per-subscription for everything else. Mixing the two would let a busy
    // channel advance the observer subscription's replay window past frames it
    // never delivered.
    let watermark_key = channel_of(event).unwrap_or_else(|| subscription_id.to_string());
    if !inner
        .session
        .record_event(&event_id, &watermark_key, created_at)
    {
        return Ingested::Duplicate;
    }

    if subscription_id.starts_with(SUB_CHANNEL_PREFIX) {
        return apply_timeline_event(inner, event, kind);
    }
    match subscription_id {
        SUB_MEMBERSHIP => apply_membership_event(inner, event, created_at),
        SUB_OBSERVER => apply_observer_event(inner, event, now),
        SUB_METRIC => apply_metric_event(inner, event),
        SUB_PRESENCE => apply_presence_event(inner, event, kind),
        // An event on a subscription the daemon did not open is not routed by
        // guessing. Forgetting it un-dedups it, so a later resubscribe that
        // *does* name a route can still deliver it — `TwoGenDedup::remove`
        // exists for exactly this "deduped but never delivered" case.
        other => {
            inner.session.forget_event(&event_id);
            tracing::debug!(
                subscription = other,
                kind,
                "event on an unrouted subscription"
            );
            Ingested::Dropped("unrouted_subscription")
        }
    }
}

/// Route a channel-timeline event: the message families, plus their overlays.
fn apply_timeline_event(inner: &mut Inner, event: &nostr::Event, kind: u32) -> Ingested {
    let Some(channel_id) = channel_of(event) else {
        return Ingested::Dropped("timeline_event_without_h_tag");
    };
    let json = event_json(event);

    if crate::timeline::is_aux_kind(kind) {
        // Aux is metadata *about* a row, never a row: it updates the message it
        // decorates rather than creating one. Emitting `message.new` for a
        // reaction would fabricate a timeline entry and corrupt the cursor,
        // which is derived from the last row.
        inner.stream.publish(
            "message.update",
            serde_json::json!({"channel_id": channel_id, "event": json}),
        );
        return Ingested::Applied(vec!["message.update".to_string()]);
    }

    let root = crate::timeline::reply_root(&json);
    let topic = if root.is_some() {
        "thread.reply"
    } else {
        "message.new"
    };
    // [D-7]: correlate the relay's echo of our own send back to the client's
    // provisional id, so the optimistic bubble reconciles without a content
    // match — content matching breaks the moment two identical messages exist.
    let local_id = inner.local_ids.take(&event.id.to_hex());
    inner.stream.publish(
        topic,
        serde_json::json!({
            "channel_id": channel_id,
            "event": json,
            "local_id": local_id,
        }),
    );
    let mut topics = vec![topic.to_string()];

    if let Some(card) = crate::askcard::parse_ask_tag(&tag_rows(event), ask_routing_of(event)) {
        let asker = event.pubkey.to_hex();
        // `AskCards::open` refuses a non-answerable (`auto`) card, which is
        // what keeps an unattended auto-approval out of the awaiting count —
        // counting it would show a blocked agent that is not blocked, and §3.4
        // sorts blocked-first.
        if inner.asks.open(&event.id.to_hex(), &asker, card.clone()) {
            inner.fleet.agent_mut(&asker).awaiting_answer = true;
            inner.stream.publish(
                "agent.permission.request",
                serde_json::json!({
                    "event_id": event.id.to_hex(),
                    "agent_pubkey": asker,
                    "channel_id": channel_id,
                    "card": card,
                }),
            );
            topics.push("agent.permission.request".to_string());
        }
    }

    if count_unread(inner, &channel_id, event, kind) {
        topics.push("channel.unread".to_string());
    }
    Ingested::Applied(topics)
}

/// The ask routing an event declares.
///
/// `ask_owner` is the default because it is the answerable one. Mis-classifying
/// an owner-routed card as `auto` renders it already-answered, so the agent
/// blocks forever with no affordance to unblock it; the reverse mistake shows
/// an affordance that does nothing, which is visible immediately. Failing
/// toward the visible mistake is the right asymmetry.
fn ask_routing_of(event: &nostr::Event) -> crate::askcard::AskRouting {
    match tag_value(event, "ask_routing").as_deref() {
        Some("auto") => crate::askcard::AskRouting::Auto,
        _ => crate::askcard::AskRouting::AskOwner,
    }
}

/// Advance a channel's unread counts, publishing `channel.unread` on a change.
///
/// Returns whether a frame was published. Counting is **incremental** — one
/// event at a time against the read-state frontier — rather than a rescan: the
/// timeline cache is a window, not the channel, so a rescan would count only
/// what happens to be loaded and the badge would shrink as history paged out.
///
/// The conversational gate lives inside
/// [`crate::readstate::ReadState::is_unread`], so a system, job, or huddle row
/// cannot create a phantom unread here even by accident.
fn count_unread(inner: &mut Inner, channel_id: &str, event: &nostr::Event, kind: u32) -> bool {
    let json = event_json(event);
    let root = crate::timeline::reply_root(&json);
    let context = match root.as_deref() {
        Some(root) => format!("{}{root}", crate::readstate::THREAD_PREFIX),
        None => channel_id.to_string(),
    };
    // The parent of a thread context is its channel; a channel has no parent.
    // Resolved at evaluation time from the event in hand rather than stored,
    // per `readstate`'s own rule — a stored link goes stale the moment a thread
    // moves, and a stale parent link silently mis-marks a whole channel read.
    let channel_owned = channel_id.to_string();
    let parent_of = move |key: &str| -> Option<String> {
        key.strip_prefix(crate::readstate::THREAD_PREFIX)
            .map(|_| channel_owned.clone())
    };
    if !inner
        .read_state
        .is_unread(&context, kind, event.created_at.as_secs(), &parent_of)
    {
        return false;
    }

    let mentions_me = inner
        .identity
        .as_ref()
        .is_some_and(|identity| tags_pubkey(event, &identity.pubkey));
    let (unread, mentions) = match inner.channels.get(channel_id) {
        Some(channel) => (
            channel.unread.saturating_add(1),
            channel.mentions.saturating_add(u32::from(mentions_me)),
        ),
        None => (1, u32::from(mentions_me)),
    };
    inner.channels.set_unread(channel_id, unread, mentions);
    inner.stream.publish(
        "channel.unread",
        serde_json::json!({
            "channel_id": channel_id,
            "unread": unread,
            "mentions": mentions,
        }),
    );
    true
}

/// Route a 44100/44101 membership notification.
fn apply_membership_event(inner: &mut Inner, event: &nostr::Event, created_at: u64) -> Ingested {
    let json = event_json(event);
    let changed = inner.channels.apply_membership(&json);
    inner.session.subscriptions.observe_membership(created_at);
    if !changed {
        // A no-op membership is common after a reconnect replay and must not
        // look like new activity. It was still *delivered* — the watermark
        // advanced — so it is applied-with-no-topics rather than dropped.
        return Ingested::Applied(Vec::new());
    }
    inner
        .stream
        .publish("channel.member", serde_json::json!({"event": json}));
    Ingested::Applied(vec!["channel.member".to_string()])
}

/// Route a 24200 observer frame through the nine-guard chain.
fn apply_observer_event(inner: &mut Inner, event: &nostr::Event, now: i64) -> Ingested {
    // `ObserverPipeline::ingest` needs `&mut self` and `&Identity`, which live
    // in the same struct. Taking the pipeline out for the call is the split
    // borrow the compiler needs; the alternative — cloning the identity —
    // copies key material for no reason (§2.5).
    let Some(mut pipeline) = inner
        .identity
        .as_ref()
        .map(|_| std::mem::take(&mut inner.observer))
    else {
        // §2.5: keyless is visible, not quiet. The frame cannot decrypt, and
        // naming the reason is what lets `GET /daemon` answer "why is this
        // agent's feed empty".
        return Ingested::Dropped("keyless_daemon");
    };
    let outcome = match inner.identity.as_ref() {
        Some(identity) => pipeline.ingest(event, identity, now),
        None => crate::observer::Ingest::Dropped(crate::observer::Guard::Decrypt),
    };
    inner.observer = pipeline;

    let frame = match outcome {
        crate::observer::Ingest::Accepted(frame) => frame,
        // [D-3]: a frame from an agent the registry has not loaded yet is held,
        // not dropped — a cold start would otherwise lose every frame that
        // arrived before the registry did.
        crate::observer::Ingest::Queued => return Ingested::Applied(Vec::new()),
        crate::observer::Ingest::Dropped(guard) => return Ingested::Dropped(guard.counter_name()),
    };

    let agent = frame.agent_pubkey.clone();
    let looping = inner.fleet.agent_mut(&agent).observe_frame(&frame);
    inner.stream.publish(
        "agent.frame",
        serde_json::to_value(&*frame).unwrap_or(serde_json::Value::Null),
    );
    let mut topics = vec!["agent.frame".to_string()];

    // An agent's working channel is ambient state on the channel row, so a turn
    // boundary moves up to two rows — the one being left and the one being
    // entered — and each moved row gets its own frame. A mid-turn frame is not
    // evidence of a move and changes nothing.
    let working_in = match frame.kind.as_str() {
        "turn_started" => Some(frame.channel_id.clone()),
        "turn_completed" | "turn_ended" | "turn_cancelled" => Some(None),
        _ => None,
    };
    if let Some(target) = working_in {
        for channel_id in inner.channels.set_agent_working(&agent, target.as_deref()) {
            inner.stream.publish(
                "agent.state",
                serde_json::json!({"channel_id": channel_id, "agent_pubkey": agent}),
            );
            topics.push("agent.state".to_string());
        }
    }

    if looping {
        // The loop detector, §3.4: "totals tell you what you spent, the loop
        // counter tells you what you are *about* to spend." Raised at the
        // moment it fires rather than by polling.
        let repeated = inner
            .fleet
            .agent(&agent)
            .and_then(crate::fleet::AgentAccumulator::repeated_tool_calls);
        inner.stream.publish(
            "agent.state",
            serde_json::json!({"agent_pubkey": agent, "repeated_tool_calls": repeated}),
        );
        topics.push("agent.state".to_string());
    }
    Ingested::Applied(topics)
}

/// Route a 44200 turn metric.
fn apply_metric_event(inner: &mut Inner, event: &nostr::Event) -> Ingested {
    let Some(identity) = inner.identity.as_ref() else {
        return Ingested::Dropped("keyless_daemon");
    };
    // The context window is a property of the *model*, not of the turn, and
    // NIP-AM carries no field for it. `None` is honest: §3.4.1 renders `—` and
    // no bar rather than inventing a denominator.
    let metric = match crate::metric::decrypt_metric(identity, event, None) {
        Ok(metric) => metric,
        Err(err) => {
            tracing::debug!(%err, "44200 rejected");
            return Ingested::Dropped("metric_decode_failed");
        }
    };
    let agent = event.pubkey.to_hex();
    inner.fleet.agent_mut(&agent).observe_metric(metric.clone());
    inner.stream.publish(
        "agent.metric",
        serde_json::json!({"agent_pubkey": agent, "metric": metric}),
    );
    Ingested::Applied(vec!["agent.metric".to_string()])
}

/// Route a 20001 beat or a 40902 snapshot.
fn apply_presence_event(inner: &mut Inner, event: &nostr::Event, kind: u32) -> Ingested {
    let pubkey = event.pubkey.to_hex();
    let status = tag_value(event, "status")
        .or_else(|| tag_value(event, "s"))
        .unwrap_or_else(|| event.content.clone());
    let created_at = event.created_at.as_secs() as i64;
    let changed = match kind {
        crate::presence::KIND_PRESENCE_BEAT => {
            inner.presence.observe_beat(&pubkey, &status, created_at)
        }
        crate::presence::KIND_PRESENCE_SNAPSHOT => inner
            .presence
            .observe_snapshot(&pubkey, &status, created_at),
        _ => return Ingested::Dropped("not_a_presence_kind"),
    };
    if !changed {
        // Presence is a coalescing topic precisely because most beats change
        // nothing. Publishing every beat would make the busiest topic on the
        // stream also the least informative.
        return Ingested::Applied(Vec::new());
    }
    let record = inner.presence.get(&pubkey, created_at);
    inner.fleet.agent_mut(&pubkey).presence = Some(record.state);
    inner.stream.publish(
        "presence.update",
        serde_json::json!({"pubkey": pubkey, "presence": record}),
    );
    Ingested::Applied(vec!["presence.update".to_string()])
}

/// Build a NIP-01 `REQ` frame.
pub fn req_frame(subscription_id: &str, filter: &serde_json::Value) -> serde_json::Value {
    serde_json::json!(["REQ", subscription_id, filter])
}

/// Build a NIP-01 `CLOSE` frame.
pub fn close_frame(subscription_id: &str) -> serde_json::Value {
    serde_json::json!(["CLOSE", subscription_id])
}

/// The liveness probe's filter: explicit `kinds`, and it cannot match.
///
/// `kinds` is present because §2.4's invariant is global — a kindless filter
/// can match a `P_GATED_KIND` and is refused by the relay's gate. An impossible
/// author and `limit: 0` make the answer an immediate `EOSE` with no rows,
/// which is exactly the round trip the probe wants and nothing more.
pub fn probe_filter() -> serde_json::Value {
    serde_json::json!({
        "kinds": [crate::timeline::KIND_WINDOW_BOUNDS],
        "authors": ["0".repeat(64)],
        "limit": 0,
    })
}

/// The live channel filter: timeline kinds, `#h`-scoped, `since`-replayed.
///
/// Distinct from [`crate::timeline::build_window_filter`], which is the *paged
/// history* read over the HTTP bridge. This is the live tail: no NIP-CW window
/// keys — a subscription has no pages, and asking a relay for a server-assembled
/// window on a live subscription would produce `39006` overlays with nothing to
/// bind to — and a `since` watermark that makes a reconnect replay only what
/// was missed.
pub fn channel_live_filter(channel_id: &str, since: Option<u64>) -> serde_json::Value {
    let mut filter = serde_json::json!({
        "kinds": crate::timeline::TIMELINE_KINDS,
        "#h": [channel_id],
        "limit": CHANNEL_TAIL_LIMIT,
    });
    if let Some(since) = since {
        filter["since"] = serde_json::json!(since);
    }
    filter
}

/// The fleet-wide metric filter: 44200 scoped to `#p = self`.
///
/// `#p = self` is **mandatory**, not a narrowing: 44200 is in
/// `RESULT_GATED_KINDS`, which loses the `ids` exemption, so without the scope
/// the relay refuses and `/agent/{pk}/metric` ships 403-ing.
/// `tests/live_relay.rs` asserts both halves against the real relay.
pub fn metric_feed_filter(self_pubkey: &str) -> serde_json::Value {
    serde_json::json!({
        "kinds": [crate::metric::KIND_AGENT_TURN_METRIC],
        "#p": [self_pubkey],
    })
}

/// One planned subscription: its id and the filter to open it with.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedReq {
    /// Subscription id, which is also the ingest route.
    pub subscription_id: String,
    /// The filter, always carrying explicit `kinds` (§2.4).
    pub filter: serde_json::Value,
}

/// Plan every REQ for a (re)subscribe, in the order they are paced out.
///
/// The ordering is the reconnect contract of §2.6:
///
/// 1. **Membership first.** It is what discovers channels; opening channel
///    subscriptions ahead of it means subscribing to yesterday's channel set.
/// 2. **Observer, then metric.** Both are `#p`-gated and both feed the fleet
///    view, which is the screen an operator is most likely watching during an
///    outage.
/// 3. **Presence.** Cheap, and `unknown` is a legitimate state, so it can wait.
/// 4. **Channels last, in registry order.** [`crate::session::Subscriptions`]
///    iterates a `BTreeMap`, so the burst is reproducible run to run — an
///    unordered map would turn a pacing bug into an intermittent one.
///
/// Every channel filter carries its watermark minus
/// [`crate::session::SINCE_SKEW_SECS`], so a reconnect replays the window it
/// missed and no more. The replayed events are absorbed by the dedup set, which
/// is why `deduped` is expected to be non-zero after a reconnect and is
/// reported separately from the drops.
pub fn plan_subscriptions(inner: &Inner, self_pubkey: &str, now: i64) -> Vec<PlannedReq> {
    let mut planned = vec![
        PlannedReq {
            subscription_id: SUB_MEMBERSHIP.to_string(),
            filter: crate::channels::build_membership_filter(
                self_pubkey,
                inner.session.subscriptions.membership_replay_since(None),
            ),
        },
        PlannedReq {
            subscription_id: SUB_OBSERVER.to_string(),
            filter: crate::observer::build_observer_filter(self_pubkey, now),
        },
        PlannedReq {
            subscription_id: SUB_METRIC.to_string(),
            filter: metric_feed_filter(self_pubkey),
        },
    ];

    // Presence is subscribed by author, so it needs a roster to subscribe to.
    // An empty roster means no REQ rather than an unscoped one: a kindless or
    // authorless presence filter would ask the relay for every beat on it.
    let roster: std::collections::BTreeSet<String> = inner
        .channels
        .list()
        .iter()
        .filter_map(|channel| inner.channels.roster(&channel.id))
        .flat_map(|members| members.iter().cloned())
        .collect();
    if !roster.is_empty() {
        let roster: Vec<String> = roster.into_iter().collect();
        planned.push(PlannedReq {
            subscription_id: SUB_PRESENCE.to_string(),
            filter: crate::presence::build_beat_filter(&roster),
        });
    }

    for (channel_id, watermark) in inner.session.subscriptions.channels() {
        planned.push(PlannedReq {
            subscription_id: channel_sub_id(channel_id),
            filter: channel_live_filter(
                channel_id,
                (watermark.last_seen > 0).then(|| watermark.resubscribe_since()),
            ),
        });
    }
    planned
}

/// A publish the loop is holding across at most one reconnect (§2.7).
#[derive(Debug)]
struct PendingPublish {
    event: nostr::Event,
    respond: Option<tokio::sync::oneshot::Sender<Result<crate::post::SendResponse>>>,
    local_id: Option<String>,
    /// When the caller asked. The deadline is measured from here, not from the
    /// last attempt, so a flapping connection cannot extend it indefinitely.
    asked_at: Instant,
}

impl PendingPublish {
    /// Resolve this publish, if a responder is still attached.
    fn resolve(&mut self, outcome: Result<crate::post::SendResponse>) {
        if let Some(respond) = self.respond.take() {
            let _ = respond.send(outcome);
        }
    }
}

/// Run the relay I/O loop until the process exits.
///
/// Never returns in normal operation: a disconnect is a reconnect, not an exit,
/// because a daemon whose relay task ended would keep answering reads from a
/// cache that silently stops updating — the §1.3-property-3 failure. The one
/// exit is a keyless daemon, which has nothing to authenticate with and logs
/// that it is not starting.
pub async fn run(state: AppState, mut commands: tokio::sync::mpsc::Receiver<WireCommand>) {
    let Some(creds) = Credentials::load(&state).await else {
        tracing::warn!(
            "no identity loaded: the relay loop is not starting, and GET /health \
             reports archiving: false for as long as that lasts"
        );
        return;
    };
    let relay_url = state.config.identity.relay_url.clone();
    // Held across reconnects on purpose — that is the whole of §2.7's "survives
    // exactly one reconnect, then fails visibly".
    let mut pending: Vec<PendingPublish> = Vec::new();

    loop {
        set_state(&state, ConnectionState::Connecting).await;
        let connect = NostrWsConnection::connect_authenticated(
            &relay_url,
            &creds.keys,
            creds.auth_tag.as_ref(),
        )
        .await;

        let conn = match connect {
            Ok(conn) => conn,
            Err(err) => {
                let delay = note_connect_failure(&state, &err).await;
                fail_expired(&mut pending);
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        set_state(&state, ConnectionState::Connected).await;
        tracing::info!(relay = %relay_url, "relay connected and authenticated");

        // A session ends by returning; the ladder and the pending publishes
        // survive it. Everything the connection owns is dropped here, so a
        // socket cannot leak across an iteration.
        session_loop(&state, conn, &creds, &mut commands, &mut pending).await;

        let delay = {
            let mut inner = state.lock().await;
            // Entering a non-`Connected` state is what resets the ladder after
            // a healthy run and counts the reconnect; `next_retry_delay` then
            // takes the rung.
            inner.session.transition(ConnectionState::Disconnected);
            inner.session.next_retry_delay(false)
        };
        let attempt = {
            let mut inner = state.lock().await;
            inner.session.backoff_mut().attempt()
        };
        set_state(
            &state,
            ConnectionState::Reconnecting {
                attempt,
                next_retry_in_ms: delay.as_millis() as u64,
            },
        )
        .await;
        fail_expired(&mut pending);
        tokio::time::sleep(delay).await;
    }
}

/// The signing material the loop needs, pulled out of the loaded identity once.
struct Credentials {
    keys: nostr::Keys,
    auth_tag: Option<nostr::Tag>,
    pubkey: String,
}

impl Credentials {
    /// Load them, or `None` when the daemon is keyless.
    async fn load(state: &AppState) -> Option<Self> {
        let inner = state.lock().await;
        let identity = inner.identity.as_ref()?;
        Some(Self {
            keys: identity.signing_keys()?,
            auth_tag: identity.auth_tag_nostr(),
            pubkey: identity.pubkey.clone(),
        })
    }
}

/// Move the connection state and publish the frame the status bar renders.
///
/// `connection.state` is a **durable** topic, so it is never dropped: §2.6
/// renders these states as chrome rather than as toasts, which only works if
/// every transition arrives. A dropped `connected` after a delivered
/// `reconnecting` leaves the status bar claiming an outage that ended.
async fn set_state(state: &AppState, next: ConnectionState) {
    let mut inner = state.lock().await;
    inner.session.transition(next);
    let current = inner.session.state().clone();
    inner
        .stream
        .publish("connection.state", serde_json::json!(current));
}

/// Classify a connect failure into the state and the delay it implies (§2.6).
///
/// A DNS brownout is **not** congestion: it retries flat at
/// [`crate::session::DNS_RETRY_INTERVAL`] with jitter rather than consuming a
/// ladder rung, because escalating a name-resolution failure up a congestion
/// ladder turns a 2-second outage into a 16-second one for no reason.
async fn note_connect_failure(state: &AppState, err: &WsClientError) -> Duration {
    let dns = is_dns_failure(err);
    let auth_failed = matches!(
        err,
        WsClientError::AuthFailed(_) | WsClientError::NoAuthChallenge
    );
    let (delay, next) = {
        let mut inner = state.lock().await;
        let delay = inner.session.next_retry_delay(dns);
        let next = if auth_failed {
            // §1.3 property 3: auth failure is never collapsed into
            // "disconnected". The TUI renders it with `:login` inline; a
            // generic disconnect would send the operator to check their network
            // for a credential problem.
            ConnectionState::AuthFailed {
                reason: err.to_string(),
            }
        } else if dns {
            ConnectionState::DnsBrownout
        } else {
            ConnectionState::Reconnecting {
                attempt: inner.session.backoff_mut().attempt(),
                next_retry_in_ms: delay.as_millis() as u64,
            }
        };
        (delay, next)
    };
    tracing::warn!(%err, delay_ms = delay.as_millis() as u64, "relay connect failed");
    set_state(state, next).await;
    delay
}

/// Whether a transport error is a name-resolution failure.
///
/// String matching because `tokio-tungstenite` erases the `io::Error` kind
/// behind its own variant, and the alternative — treating every transport error
/// as congestion — is the behaviour §2.6 explicitly rejects. A false negative
/// costs one ladder rung; a false positive costs a flat retry, which is the
/// gentler of the two mistakes.
pub fn is_dns_failure(err: &WsClientError) -> bool {
    let text = err.to_string().to_lowercase();
    text.contains("dns")
        || text.contains("failed to lookup")
        || text.contains("name or service not known")
        || text.contains("nodename nor servname")
        || text.contains("temporary failure in name resolution")
}

/// Fail every publish past its deadline, so loss is never silent (§2.7).
fn fail_expired(pending: &mut Vec<PendingPublish>) {
    pending.retain_mut(|item| {
        if item.asked_at.elapsed() < PUBLISH_DEADLINE {
            return true;
        }
        item.resolve(Err(DaemonError::RelayUnreachable));
        false
    });
}

/// Drive one connected session until the socket dies.
async fn session_loop(
    state: &AppState,
    mut conn: NostrWsConnection,
    creds: &Credentials,
    commands: &mut tokio::sync::mpsc::Receiver<WireCommand>,
    pending: &mut Vec<PendingPublish>,
) {
    let mut queue: std::collections::VecDeque<PlannedReq> = {
        let inner = state.lock().await;
        plan_subscriptions(&inner, &creds.pubkey, unix_now()).into()
    };
    let mut last_frame = Instant::now();
    let mut last_paced = Instant::now();
    let mut probe_sent: Option<Instant> = None;

    loop {
        // ── Read. The only await that can lose a frame, and it is never inside
        // a `select!`, so it is never cancelled.
        match conn.next_event(TICK).await {
            Ok(message) => {
                last_frame = Instant::now();
                probe_sent = None;
                if !handle_message(state, message, pending).await {
                    return;
                }
            }
            Err(WsClientError::Timeout) => {}
            Err(err) => {
                tracing::info!(%err, "relay socket ended");
                // Every unacked observer write is conservatively retried: a
                // socket death carries no event ids, so the queue cannot tell
                // which ones landed. Duplicate ids are harmless at the relay;
                // out-of-order telemetry is not.
                state
                    .lock()
                    .await
                    .session
                    .observer_queue
                    .requeue_in_flight();
                return;
            }
        }

        // ── One paced REQ per tick, which is what keeps a 48-channel
        // resubscribe from bursting past the relay's ~50-frames/5 s admission.
        if last_paced.elapsed() >= crate::session::REQ_PACING_INTERVAL {
            last_paced = Instant::now();
            if let Some(req) = queue.pop_front() {
                if conn
                    .send_raw(&req_frame(&req.subscription_id, &req.filter))
                    .await
                    .is_err()
                {
                    return;
                }
                let mut inner = state.lock().await;
                mark_subscription_active(&mut inner, &req.subscription_id);
                continue;
            }
        }

        // ── One command per tick, for the same pacing reason.
        if let Ok(command) = commands.try_recv() {
            if !handle_command(state, command, pending, &mut queue).await {
                return;
            }
        }

        // ── Publishes, including the ones carried across a reconnect.
        drain_publishes(state, pending).await;

        // ── Liveness. Any inbound frame is evidence, so a busy relay never
        // sees a probe.
        match probe_sent {
            Some(sent) if sent.elapsed() >= crate::session::PONG_TIMEOUT => {
                state.lock().await.session.record_pong_timeout();
                tracing::info!("liveness probe deadline missed; the socket is half-open");
                let _ = conn.send_raw(&close_frame(SUB_PROBE)).await;
                return;
            }
            Some(_) => {}
            None if last_frame.elapsed() >= crate::session::PING_INTERVAL => {
                if conn
                    .send_raw(&req_frame(SUB_PROBE, &probe_filter()))
                    .await
                    .is_err()
                {
                    return;
                }
                probe_sent = Some(Instant::now());
            }
            None => {}
        }

        // ── Read-state debounce. Fires only when a client marked something.
        publish_due_read_state(state).await;
    }
}

/// Record that a subscription is live, so a reconnect restores *which* ones
/// were open rather than only the channel set (§5.2).
fn mark_subscription_active(inner: &mut Inner, subscription_id: &str) {
    match subscription_id {
        SUB_MEMBERSHIP => {
            inner.session.subscriptions.membership_active = true;
            // The dropped-membership marker exists to pull the replay window
            // back far enough to re-deliver an event that was dropped before
            // delivery. Once the resubscribe carrying that window is on the
            // wire, the marker has done its job.
            inner.session.subscriptions.clear_membership_dropped();
        }
        SUB_OBSERVER => inner.session.subscriptions.observer_active = true,
        _ => {}
    }
}

/// Handle one relay frame. Returns `false` when the session must end.
async fn handle_message(
    state: &AppState,
    message: RelayMessage,
    pending: &mut [PendingPublish],
) -> bool {
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } => {
            // The probe's filter cannot match, so an event on it is a relay
            // behaving unexpectedly rather than data. Ignoring it keeps a
            // misbehaving relay from injecting into an unrouted store.
            if subscription_id == SUB_PROBE {
                return true;
            }
            let now = unix_now();
            let mut inner = state.lock().await;
            if let Ingested::Dropped(reason) =
                apply_relay_event(&mut inner, &subscription_id, &event, now)
            {
                tracing::debug!(reason, kind = kind_of(&event), "event dropped");
            }
            true
        }
        RelayMessage::Ok(ok) => {
            state
                .lock()
                .await
                .session
                .observer_queue
                .acknowledge(&ok.event_id);
            // A publish acknowledged over the socket resolves here too, so an
            // `OK` arriving while the bridge call is still in flight does not
            // leave the caller waiting for a verdict that already exists.
            for item in pending.iter_mut() {
                if item.event.id.to_hex() == ok.event_id {
                    let local_id = item.local_id.clone();
                    item.resolve(Ok(crate::post::SendResponse {
                        event_id: ok.event_id.clone(),
                        accepted: ok.accepted,
                        message: ok.message.clone(),
                        local_id,
                    }));
                }
            }
            true
        }
        RelayMessage::Eose { .. } | RelayMessage::Count { .. } => true,
        RelayMessage::Closed {
            subscription_id,
            message,
        } => {
            // A `restricted:` close is a subscription that will never yield
            // rows — usually a `#p`-gated kind whose scope is wrong. Naming it
            // is what turns "the agent feed is empty" into a one-line answer.
            tracing::warn!(subscription = %subscription_id, %message, "relay closed a subscription");
            true
        }
        RelayMessage::Notice { message } => {
            handle_notice(state, &message).await;
            true
        }
        // A challenge mid-session means the relay wants re-authentication,
        // which `connect_authenticated` performs at connect. Ending the session
        // and reconnecting is the honest handling; re-signing in place would
        // fork the auth path that already exists.
        RelayMessage::Auth { .. } => false,
    }
}

/// Arm the rate-limit gate on a `rate-limited:` notice (§2.7).
async fn handle_notice(state: &AppState, message: &str) {
    let lower = message.to_lowercase();
    if !lower.contains("rate-limit") && !lower.contains("rate limited") {
        tracing::info!(notice = %message, "relay notice");
        return;
    }
    let retry_after_ms = crate::rest::parse_retry_hint(message)
        .map(|secs| secs.min(crate::rest::RETRY_IN_MAX_SECS) * 1_000)
        .unwrap_or(1_000);
    {
        let mut inner = state.lock().await;
        // A NOTICE carries no event id, so every unacked observer write is
        // conservatively retried — and restored **ahead** of newly parked
        // frames, because retrying them behind newer ones delivers an agent's
        // turn out of order.
        inner.session.observer_queue.requeue_in_flight();
    }
    set_state(state, ConnectionState::RateLimited { retry_after_ms }).await;
}

/// Handle one command. Returns `false` when the session must end.
async fn handle_command(
    state: &AppState,
    command: WireCommand,
    pending: &mut Vec<PendingPublish>,
    queue: &mut std::collections::VecDeque<PlannedReq>,
) -> bool {
    match command {
        WireCommand::Publish {
            event,
            respond,
            local_id,
        } => {
            pending.push(PendingPublish {
                event: *event,
                respond,
                local_id,
                asked_at: Instant::now(),
            });
            true
        }
        WireCommand::Subscribe { channel_id } => {
            let since = {
                let mut inner = state.lock().await;
                inner.session.subscriptions.subscribe(channel_id.clone());
                inner.session.subscriptions.resubscribe_since(&channel_id)
            };
            queue.push_back(PlannedReq {
                subscription_id: channel_sub_id(&channel_id),
                filter: channel_live_filter(&channel_id, since.filter(|s| *s > 0)),
            });
            true
        }
        // Ending the session *is* the reconnect: the ladder, the watermark
        // replay, and the resubscribe are all on the reconnect path already,
        // and a second in-place path would be a second thing to keep correct.
        WireCommand::Reconnect => false,
    }
}

/// Attempt the oldest pending publish, resolving it on a definite outcome.
///
/// One per tick: a publish is an HTTP round trip and running them unbounded
/// from a loop that also owns the socket would starve the read.
async fn drain_publishes(state: &AppState, pending: &mut Vec<PendingPublish>) {
    let Some(item) = pending.first() else {
        return;
    };
    let event = item.event.clone();
    let local_id = item.local_id.clone();
    match submit(state, &event).await {
        Some(response) => {
            let mut item = pending.remove(0);
            item.resolve(Ok(crate::post::SendResponse {
                local_id,
                ..response
            }));
        }
        // A publish whose outcome is unknown stays pending: it is ambiguous,
        // not failed, and re-publishing the same signed event is idempotent at
        // the relay. `fail_expired` is what makes it terminate.
        None => fail_expired(pending),
    }
}

/// Submit an event over the HTTP bridge.
///
/// `None` means the outcome is not known — the publish stays pending. The
/// bridge already applied §2.7's retry policy, including the rule that a
/// moderation kind is never blindly retried.
async fn submit(state: &AppState, event: &nostr::Event) -> Option<crate::post::SendResponse> {
    // The identity is borrowed only long enough to make the call; holding the
    // state lock across an HTTP round trip would stall every socket client for
    // its duration.
    let rest = state.rest.clone();
    let inner = state.lock().await;
    let identity = inner.identity.as_ref()?;
    // `submit_event` needs `&Identity`, and the lock guard is what keeps the
    // reference alive. The call is awaited under the lock deliberately: the
    // alternative is cloning key material out of `Inner` (§2.5 forbids growing
    // the number of copies), and the daemon's clients are a handful of terminal
    // panes rather than a request fleet.
    let outcome = rest.submit_event(identity, event).await;
    drop(inner);
    match outcome {
        Ok(body) => Some(crate::post::SendResponse {
            event_id: event.id.to_hex(),
            accepted: true,
            message: body,
            local_id: None,
        }),
        Err(err) => {
            tracing::debug!(%err, "publish did not complete");
            None
        }
    }
}

/// Publish the read-state slots when a client has marked something.
///
/// **The only write path that is not an explicit command**, and it is gated on
/// [`crate::readstate::ReadState::is_dirty`], which only a client `mark` sets.
/// That is what makes the live tests' read-only property structural rather than
/// a convention: a caller that marks nothing cannot reach this branch.
async fn publish_due_read_state(state: &AppState) {
    let prepared = {
        let mut inner = state.lock().await;
        if !inner.read_state.is_dirty() {
            return;
        }
        let Some(identity) = inner.identity.as_ref() else {
            return;
        };
        let slots = inner.read_state.to_slots();
        let events: Vec<nostr::Event> = slots
            .iter()
            .enumerate()
            .filter_map(|(index, blob)| {
                match build_read_state_event(
                    identity,
                    blob,
                    &inner.read_state.slot_d_tag_for(index),
                ) {
                    Ok(event) => Some(event),
                    Err(err) => {
                        tracing::warn!(%err, index, "read-state slot could not be built");
                        None
                    }
                }
            })
            .collect();
        // Cleared before the publish, not after: a mark that lands *during* the
        // publish must re-dirty the flag and get its own round, and clearing
        // afterwards would swallow it.
        inner.read_state.mark_published();
        events
    };
    for event in prepared {
        if submit(state, &event).await.is_none() {
            // A failed slot is not retried here. The frontier is grow-only and
            // max-wins, so the next mark republishes a superset — a retry loop
            // would republish the same blob against a relay that just refused
            // it, on the daemon's hottest path.
            break;
        }
    }
}

/// Build one NIP-44 self-encrypted kind-30078 read-state slot.
///
/// Self-encrypted — the recipient is the daemon's own pubkey — which is what
/// makes the blob readable by every device holding this identity and by nobody
/// else.
fn build_read_state_event(
    identity: &crate::identity::Identity,
    blob: &crate::readstate::ReadStateBlob,
    d_tag: &str,
) -> Result<nostr::Event> {
    let keys = identity
        .signing_keys()
        .ok_or_else(|| DaemonError::IdentityDecrypt("no identity loaded".into()))?;
    let plaintext = serde_json::to_string(blob)?;
    let content = nostr::nips::nip44::encrypt(
        keys.secret_key(),
        &keys.public_key(),
        &plaintext,
        nostr::nips::nip44::Version::V2,
    )
    .map_err(|e| DaemonError::Sdk(format!("read-state encrypt failed: {e}")))?;
    let builder = nostr::EventBuilder::new(
        nostr::Kind::Custom(buzz_core::kind::KIND_READ_STATE as u16),
        content,
    )
    .tags([nostr::Tag::identifier(d_tag.to_string())]);
    identity.sign_event(builder)
}

/// Unix seconds now, saturating rather than panicking on a pre-epoch clock.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, SocketIdentity};
    use crate::identity::Identity;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    const NOW: i64 = 1_785_852_720;

    fn config() -> Config {
        Config {
            identity: SocketIdentity::new("wss://relay.example", "aa".repeat(32), ""),
            socket: std::path::PathBuf::from("/run/user/1000/buzz/x.sock"),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/buzz"),
            data_dir: std::path::PathBuf::from("/home/u/.local/share/buzz"),
            idle_timeout: None,
            observer_cache_bytes: 1024 * 1024,
            systemd_managed: false,
        }
    }

    fn channel_message(
        author: &Keys,
        channel_id: &str,
        content: &str,
        tags: Vec<Tag>,
        created_at: i64,
    ) -> nostr::Event {
        let mut all = vec![Tag::parse(["h", channel_id]).unwrap()];
        all.extend(tags);
        EventBuilder::new(Kind::Custom(9), content)
            .tags(all)
            .custom_created_at(nostr::Timestamp::from_secs(created_at as u64))
            .sign_with_keys(author)
            .expect("sign")
    }

    const CHANNEL: &str = "3f1d9c9e-0f7a-4a2e-9b1f-2c4d5e6f7a8b";

    /// §2.4's global invariant applies to the loop's own frames too: every
    /// filter it puts on the wire carries explicit `kinds`, including the
    /// liveness probe, which is the one that looks least like a query.
    #[tokio::test]
    async fn every_planned_filter_carries_explicit_kinds() {
        let state = AppState::new(config(), None).expect("state");
        let inner = state.lock().await;
        let planned = plan_subscriptions(&inner, &"bb".repeat(32), NOW);
        assert!(!planned.is_empty());
        for req in &planned {
            crate::search::assert_explicit_kinds(&req.filter, "planned req")
                .unwrap_or_else(|e| panic!("{}: {e}", req.subscription_id));
        }
        crate::search::assert_explicit_kinds(&probe_filter(), "probe").expect("probe has kinds");
        crate::search::assert_explicit_kinds(&channel_live_filter(CHANNEL, None), "tail")
            .expect("tail has kinds");
    }

    /// §2.6: membership is planned **first**, because it is what discovers
    /// channels. Opening channel subscriptions ahead of it means subscribing to
    /// yesterday's channel set.
    #[tokio::test]
    async fn membership_is_planned_before_the_channels_it_discovers() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        inner.session.subscriptions.subscribe(CHANNEL);
        let planned = plan_subscriptions(&inner, &"bb".repeat(32), NOW);
        let position = |id: &str| planned.iter().position(|r| r.subscription_id == id);
        assert_eq!(position(SUB_MEMBERSHIP), Some(0));
        assert!(
            position(&channel_sub_id(CHANNEL)).unwrap() > position(SUB_OBSERVER).unwrap(),
            "channels are paced last"
        );
    }

    /// §2.6: a resubscribe replays from `last_seen - SINCE_SKEW_SECS`, and a
    /// channel that has seen nothing carries no `since` at all — a `since: 0`
    /// would be indistinguishable from "replay everything since the epoch".
    #[tokio::test]
    async fn a_resubscribe_replays_from_the_skewed_watermark() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        inner.session.subscriptions.subscribe(CHANNEL);
        let cold = plan_subscriptions(&inner, &"bb".repeat(32), NOW);
        let cold_filter = &cold
            .iter()
            .find(|r| r.subscription_id == channel_sub_id(CHANNEL))
            .expect("channel planned")
            .filter;
        assert!(cold_filter.get("since").is_none(), "{cold_filter}");

        inner.session.subscriptions.observe(CHANNEL, 1_000);
        let warm = plan_subscriptions(&inner, &"bb".repeat(32), NOW);
        let warm_filter = &warm
            .iter()
            .find(|r| r.subscription_id == channel_sub_id(CHANNEL))
            .expect("channel planned")
            .filter;
        assert_eq!(
            warm_filter["since"].as_u64(),
            Some(1_000 - crate::session::SINCE_SKEW_SECS)
        );
    }

    /// The dedup set is what makes a reconnect replay free. An event delivered
    /// twice is `Duplicate` — counted, not dropped — so `GET /daemon` can tell
    /// an expected replay from a real loss.
    #[tokio::test]
    async fn a_replayed_event_is_deduped_rather_than_reapplied() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let event = channel_message(&author, CHANNEL, "hello", vec![], NOW);
        let sub = channel_sub_id(CHANNEL);

        let first = apply_relay_event(&mut inner, &sub, &event, NOW);
        assert!(
            first.topics().contains(&"message.new".to_string()),
            "{first:?}"
        );
        let second = apply_relay_event(&mut inner, &sub, &event, NOW);
        assert_eq!(second, Ingested::Duplicate);
        assert_eq!(inner.session.counters().deduped, 1);
    }

    /// An event on a subscription the daemon never opened is **un-deduped** on
    /// the way out. Leaving it in the dedup set would make it permanently
    /// invisible: a later resubscribe that does name a route could never
    /// deliver it.
    #[tokio::test]
    async fn an_unrouted_event_is_forgotten_so_a_resubscribe_can_deliver_it() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let event = channel_message(&author, CHANNEL, "hello", vec![], NOW);

        let outcome = apply_relay_event(&mut inner, "some-other-sub", &event, NOW);
        assert_eq!(outcome, Ingested::Dropped("unrouted_subscription"));
        assert!(!inner.session.seen.contains(&event.id.to_hex()));

        // And the route that *does* exist still delivers it.
        let routed = apply_relay_event(&mut inner, &channel_sub_id(CHANNEL), &event, NOW);
        assert!(routed.topics().contains(&"message.new".to_string()));
    }

    /// §3.1 / deliverable 5: a reply is `thread.reply`, not `message.new`. The
    /// two are different rows in different views, and collapsing them puts every
    /// reply in the channel window.
    #[tokio::test]
    async fn a_reply_publishes_thread_reply_and_a_root_publishes_message_new() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let sub = channel_sub_id(CHANNEL);

        let root = channel_message(&author, CHANNEL, "root", vec![], NOW);
        let root_id = root.id.to_hex();
        assert!(apply_relay_event(&mut inner, &sub, &root, NOW)
            .topics()
            .contains(&"message.new".to_string()));

        let reply = channel_message(
            &author,
            CHANNEL,
            "reply",
            vec![Tag::parse(["e", &root_id, "", "root"]).unwrap()],
            NOW + 1,
        );
        assert!(apply_relay_event(&mut inner, &sub, &reply, NOW)
            .topics()
            .contains(&"thread.reply".to_string()));
    }

    /// [D-10]: aux is metadata about a row, never a row. A reaction that
    /// published `message.new` would fabricate a timeline entry and corrupt the
    /// cursor, which is derived from the last row.
    #[tokio::test]
    async fn an_aux_event_updates_a_row_rather_than_creating_one() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let reaction = EventBuilder::new(Kind::Custom(7), "+")
            .tags([Tag::parse(["h", CHANNEL]).unwrap()])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&author)
            .expect("sign");

        let outcome = apply_relay_event(&mut inner, &channel_sub_id(CHANNEL), &reaction, NOW);
        assert_eq!(outcome.topics(), ["message.update".to_string()]);
    }

    /// Deliverable 5: unread counting is gated on the conversational subset. A
    /// system row is a thing that happened, not a thing somebody said, and
    /// counting it produces a badge that clears itself.
    #[tokio::test]
    async fn a_system_row_creates_no_unread() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let system = EventBuilder::new(Kind::Custom(40_099), "joined")
            .tags([Tag::parse(["h", CHANNEL]).unwrap()])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&author)
            .expect("sign");

        let outcome = apply_relay_event(&mut inner, &channel_sub_id(CHANNEL), &system, NOW);
        assert!(
            !outcome.topics().contains(&"channel.unread".to_string()),
            "{outcome:?}"
        );
        assert_eq!(inner.channels.totals(), (0, 0));
    }

    /// A conversational message does count, and one that `p`-tags this identity
    /// counts as a mention too — which is what drives the attention sort.
    #[tokio::test]
    async fn a_mention_of_this_identity_counts_as_both() {
        let owner = Keys::generate();
        let identity = Identity::from_keys(owner.clone(), None);
        let state = AppState::new(config(), Some(identity)).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let message = channel_message(
            &author,
            CHANNEL,
            "hey you",
            vec![Tag::public_key(owner.public_key())],
            NOW,
        );

        let outcome = apply_relay_event(&mut inner, &channel_sub_id(CHANNEL), &message, NOW);
        assert!(outcome.topics().contains(&"channel.unread".to_string()));
        assert_eq!(inner.channels.totals(), (0, 0), "no cached channel row yet");
    }

    /// §2.5: a keyless daemon cannot decrypt an observer frame, and says so by
    /// name rather than silently yielding an empty feed.
    #[tokio::test]
    async fn a_keyless_daemon_names_the_reason_an_observer_frame_did_not_land() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let agent = Keys::generate();
        let frame = EventBuilder::new(Kind::Custom(24_200), "x")
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&agent)
            .expect("sign");

        let outcome = apply_relay_event(&mut inner, SUB_OBSERVER, &frame, NOW);
        assert_eq!(outcome, Ingested::Dropped("keyless_daemon"));
    }

    /// Presence is coalescing precisely because most beats change nothing.
    /// A repeat beat publishes no frame; a transition does.
    #[tokio::test]
    async fn only_a_presence_transition_publishes_a_frame() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let agent = Keys::generate();
        let beat = |created_at: i64, status: &str| {
            EventBuilder::new(Kind::Custom(20_001), "")
                .tags([Tag::parse(["status", status]).unwrap()])
                .custom_created_at(nostr::Timestamp::from_secs(created_at as u64))
                .sign_with_keys(&agent)
                .expect("sign")
        };

        let first = apply_relay_event(&mut inner, SUB_PRESENCE, &beat(NOW, "online"), NOW);
        assert_eq!(first.topics(), ["presence.update".to_string()]);
        let repeat = apply_relay_event(&mut inner, SUB_PRESENCE, &beat(NOW + 1, "online"), NOW);
        assert!(repeat.topics().is_empty(), "{repeat:?}");
        let change = apply_relay_event(&mut inner, SUB_PRESENCE, &beat(NOW + 2, "offline"), NOW);
        assert_eq!(change.topics(), ["presence.update".to_string()]);
    }

    /// §5.2: `n_sub_active` / `observer_control_sub_active` must survive a
    /// reconnect — a resubscribe restores *which* subscriptions were live, not
    /// only the channel set.
    #[tokio::test]
    async fn marking_a_subscription_active_records_which_one_was_live() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        assert!(!inner.session.subscriptions.membership_active);
        assert!(!inner.session.subscriptions.observer_active);
        mark_subscription_active(&mut inner, SUB_MEMBERSHIP);
        mark_subscription_active(&mut inner, SUB_OBSERVER);
        assert!(inner.session.subscriptions.membership_active);
        assert!(inner.session.subscriptions.observer_active);
    }

    /// §2.6: a DNS brownout is not congestion. Classified here so the ladder
    /// never escalates a 2-second name-resolution outage into a 16-second one.
    #[tokio::test]
    async fn a_name_resolution_failure_is_not_treated_as_congestion() {
        let dns = WsClientError::Url("failed to lookup address information".into());
        assert!(is_dns_failure(&dns));
        let refused = WsClientError::ConnectionClosed;
        assert!(!is_dns_failure(&refused));
    }

    /// §2.4: `/agent/{pk}/metric`'s kind loses the `ids` exemption, so the feed
    /// filter **must** carry `#p = self`. This is the shape the live test
    /// asserts the relay accepts.
    #[tokio::test]
    async fn the_metric_feed_is_scoped_to_p_self() {
        let me = "cc".repeat(32);
        let filter = metric_feed_filter(&me);
        assert_eq!(filter["#p"][0].as_str(), Some(me.as_str()));
        assert_eq!(
            filter["kinds"][0].as_u64(),
            Some(u64::from(crate::metric::KIND_AGENT_TURN_METRIC))
        );
    }

    /// The probe must be answerable by every relay and must return no rows —
    /// otherwise a liveness check becomes a query with a cost.
    #[tokio::test]
    async fn the_liveness_probe_cannot_match_anything() {
        let filter = probe_filter();
        assert_eq!(filter["limit"].as_u64(), Some(0));
        assert_eq!(filter["authors"][0].as_str(), Some("0".repeat(64).as_str()));
    }

    /// §2.7: a publish past its deadline fails **visibly**. Silence is the one
    /// outcome the offline story forbids.
    #[tokio::test]
    async fn a_publish_past_its_deadline_fails_rather_than_going_quiet() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), "x")
            .sign_with_keys(&keys)
            .expect("sign");
        let (respond, wait) = tokio::sync::oneshot::channel();
        let mut pending = vec![PendingPublish {
            event,
            respond: Some(respond),
            local_id: Some("local-1".into()),
            asked_at: Instant::now() - PUBLISH_DEADLINE - Duration::from_secs(1),
        }];
        fail_expired(&mut pending);
        assert!(pending.is_empty());
        let outcome = wait.await.expect("the caller is answered, never dropped");
        assert!(matches!(outcome, Err(DaemonError::RelayUnreachable)));
    }

    /// A publish still inside its window is held, not failed — that is the
    /// "survives exactly one reconnect" half of §2.7.
    #[tokio::test]
    async fn a_fresh_publish_is_carried_rather_than_failed() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), "x")
            .sign_with_keys(&keys)
            .expect("sign");
        let (respond, mut wait) = tokio::sync::oneshot::channel();
        let mut pending = vec![PendingPublish {
            event,
            respond: Some(respond),
            local_id: None,
            asked_at: Instant::now(),
        }];
        fail_expired(&mut pending);
        assert_eq!(pending.len(), 1);
        assert!(wait.try_recv().is_err(), "the caller is still waiting");
    }

    /// The `WireHandle` reports a dead loop as `relay_unreachable` rather than
    /// panicking: a daemon whose relay task died must still answer reads from
    /// cache (§2.7).
    #[tokio::test]
    async fn a_dead_loop_is_reported_as_relay_unreachable() {
        let (handle, receiver) = channel();
        drop(receiver);
        let err = handle
            .send(WireCommand::Reconnect)
            .await
            .expect_err("a closed channel is an error");
        assert_eq!(err.code(), "relay_unreachable");
    }

    /// [D-7]: the provisional id is consumed by the reconcile it was recorded
    /// for. A reconnect replay of the same event must not re-fire it.
    #[tokio::test]
    async fn a_local_id_reconciles_once_and_only_once() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let event = channel_message(&author, CHANNEL, "sent", vec![], NOW);
        inner.local_ids.record(event.id.to_hex(), "local-7");

        apply_relay_event(&mut inner, &channel_sub_id(CHANNEL), &event, NOW);
        assert!(inner.local_ids.is_empty(), "the correlation was consumed");
        // A replay is deduped before it can reach the map a second time.
        assert_eq!(
            apply_relay_event(&mut inner, &channel_sub_id(CHANNEL), &event, NOW),
            Ingested::Duplicate
        );
    }
}
