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

/// Minimum spacing between attempts on the same pending publish.
///
/// Without it, a failing publish is retried once per [`TICK`] — ~240 HTTP
/// requests across one [`PUBLISH_DEADLINE`], aimed at a relay that is by
/// definition already unwell. The bridge's own per-request retry ladder
/// (`RETRY_BASE_SECS`) handles the fast transients; this paces the *outer*
/// loop, which exists for the slow ones.
pub const PUBLISH_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Live-tail page size for a channel subscription.
///
/// A subscription is a *tail*, not a history read: the paged history walk is
/// [`crate::timeline::build_window_filter`] over the HTTP bridge. This bounds
/// the catch-up burst a reconnect replays before the tail goes live.
pub const CHANNEL_TAIL_LIMIT: u32 = 200;

/// Minimum spacing between cold-start discovery walks.
///
/// The walk is two HTTP round trips against the relay, and a flapping socket
/// would otherwise run it on every reconnect — a discovery storm aimed at a
/// relay that is by definition already unwell, and one that buys nothing: the
/// loop maintains the channel set live from 44100/44101 once it is seeded, so a
/// reconnect a few seconds later is looking at the same answer. A long outage
/// is different, and that is exactly what the interval admits.
pub const DISCOVERY_INTERVAL: Duration = Duration::from_secs(300);

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

    // Dedupe first, and **only** dedupe: the watermark does not move here.
    //
    // An earlier revision advanced it in the same call, before the routing
    // decision below — which meant every `Dropped` arm left the replay window
    // past an event that was deduped and never delivered. After a reconnect the
    // resubscribe would start *after* it, so the event was permanently
    // invisible: the exact "deduped but never delivered" failure
    // `TwoGenDedup::remove` exists to prevent, reached by the watermark instead
    // of the dedup set. The two now move together, at the bottom, on delivery.
    if !inner.session.seen.insert(event_id.clone()) {
        inner.session.note_duplicate();
        return Ingested::Duplicate;
    }

    let outcome = if subscription_id.starts_with(SUB_CHANNEL_PREFIX) {
        apply_timeline_event(inner, event, kind)
    } else {
        match subscription_id {
            SUB_MEMBERSHIP => apply_membership_event(inner, event, created_at),
            SUB_OBSERVER => apply_observer_event(inner, event, now),
            SUB_METRIC => apply_metric_event(inner, event),
            SUB_PRESENCE => apply_presence_event(inner, event, kind, now),
            // An event on a subscription the daemon did not open is not routed
            // by guessing.
            other => {
                tracing::debug!(
                    subscription = other,
                    kind,
                    "event on an unrouted subscription"
                );
                Ingested::Dropped("unrouted_subscription")
            }
        }
    };

    match outcome {
        // Delivered. The watermark key is the **subscription's own channel** —
        // the id after `ch:` — and nothing else.
        //
        // It used to be `channel_of(event).unwrap_or(subscription_id)`, i.e.
        // the `h` tag when present. That reads as equivalent and is not:
        // **44100/44101 membership notifications carry an `h` tag naming the
        // channel** (`buzz-relay/src/handlers/side_effects.rs:914`), so a
        // membership event delivered on `member` advanced *that channel's*
        // timeline watermark to the membership event's `created_at` — a
        // timestamp with nothing to do with the channel's messages.
        //
        // Worst on the path directly above: a fresh join registers the channel
        // at `last_seen: 0`, and the same call then moved it to now. The next
        // `plan_subscriptions` therefore emitted `since: now - SKEW` instead of
        // no `since` at all, and `CHANNEL_TAIL_LIMIT` was silently suppressed —
        // you join a channel and its timeline is empty, with no error anywhere.
        // The relay re-emits 44100 to every member on unarchive
        // (`side_effects.rs:1608`), so one unarchive did this to everybody.
        //
        // Deriving the key from the subscription rather than from the payload
        // is also the same rule the routing above already follows, and for the
        // same reason: what a frame *is* is decided by which subscription
        // delivered it, never by re-reading intent out of its tags.
        Ingested::Applied(_) => {
            let key = subscription_id
                .strip_prefix(SUB_CHANNEL_PREFIX)
                .unwrap_or(subscription_id);
            inner.session.subscriptions.observe(key, created_at);
        }
        // Dropped: un-dedupe it so a resubscribe can re-deliver it, and leave
        // the watermark where it was so the replay window still reaches back
        // far enough to include it. Both halves are required — forgetting the
        // id while the watermark has moved past the event is a no-op.
        Ingested::Dropped(_) => inner.session.forget_event(&event_id),
        Ingested::Duplicate => {}
    }
    outcome
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

    // Discovery lands in the **subscription registry**, not only in the channel
    // cache. An earlier revision updated `Channels` and stopped there, so the
    // only thing that ever called `subscriptions.subscribe` was the history
    // endpoint: a fresh daemon with twelve channels subscribed to *zero* of
    // them until a client paged history on each one, which is the same
    // permanently-empty-stores symptom this whole module exists to fix.
    //
    // Registering here is what makes the next `plan_subscriptions` include the
    // channel, and the REQ itself is paced out by the loop rather than sent
    // from this pure function.
    //
    // **Only for a join, and only for this identity.** The filter is
    // `{kinds:[44100,44101], #p:[self]}` — it carries no `#h` scope and both
    // kinds land here — so the unconditional form subscribed on *removal* too:
    // being removed from a channel registered a permanent live tail on it, one
    // the relay answers `CLOSED restricted:` to on every reconnect forever
    // (`Subscriptions::subscribe` is `or_insert` with no cap, and
    // `unsubscribe` has no production caller). A removal must do the opposite,
    // and does.
    if let Some(channel_id) = channel_of(event).or_else(|| tag_value(event, "d")) {
        if uuid::Uuid::parse_str(&channel_id).is_ok() {
            if kind_of(event) == crate::channels::KIND_MEMBER_REMOVED {
                // Only when *we* were the one removed. A 44101 can also reach
                // this daemon for a peer leaving a channel it is still in — the
                // `#p` filter matches on the notification's target, but a relay
                // is free to fan out more broadly, and dropping our own tail
                // because somebody else left would be a silent blackout of a
                // live channel. A keyless daemon has no self to compare against
                // and therefore unsubscribes from nothing, which is right: it
                // has no relay loop either.
                let is_self = inner
                    .identity
                    .as_ref()
                    .is_some_and(|identity| tags_pubkey(event, &identity.pubkey));
                if is_self {
                    inner.session.subscriptions.unsubscribe(&channel_id);
                }
            } else {
                inner.session.subscriptions.subscribe(channel_id);
            }
        }
    }

    if !changed {
        // A no-op membership is common after a reconnect replay and must not
        // look like new activity. It was still *delivered* — the watermark
        // advances — so it is applied-with-no-topics rather than dropped.
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
///
/// `now` is the **daemon's** clock, not the event's. Reading staleness against
/// the event's own `created_at` makes `now - last_seen == 0` by construction,
/// so [`crate::presence::PRESENCE_BEAT_TTL_SECS`] can never elapse and any peer
/// — including one whose beats stopped an hour ago, or one publishing a
/// backdated `created_at` — pins itself `present` forever. That is the
/// looks-alive-while-it-is-dead failure §1.3 property 3 forbids, and presence
/// is the one store whose entire job is not to make that claim.
fn apply_presence_event(inner: &mut Inner, event: &nostr::Event, kind: u32, now: i64) -> Ingested {
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
    let record = inner.presence.get(&pubkey, now);
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
    /// When the last bridge attempt was made, for [`PUBLISH_RETRY_INTERVAL`].
    /// `None` means it has never been attempted, which is what makes the first
    /// attempt immediate rather than one interval late.
    last_attempt: Option<Instant>,
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
    install_crypto_provider();
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
    // When the cold-start walk last succeeded. `None` means never, which is what
    // makes the first walk run immediately rather than one interval late.
    let mut last_discovery: Option<Instant> = None;
    // Read-state is hydrated once per process, not per reconnect: after the
    // first merge the in-memory map is ahead of or equal to the relay's.
    let mut read_state_hydrated = false;

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

        // The cold-start walk, **before** the first subscription plan. 39002 is
        // a state record and 44100/44101 are change notifications, so a settled
        // community emits nothing to hear and the registry would otherwise stay
        // empty for the life of the process. `plan_subscriptions` reads the
        // registry, so ordering is the whole of the fix: hydrating afterwards
        // would open tails on the previous set and wait for the next reconnect.
        hydrate_channels(&state, &mut last_discovery).await;
        // Before the tails open, so the unread counts the first frames land on
        // are computed against the real frontier rather than against an empty
        // one. The other order shows every message as unread for the width of
        // one query and then silently corrects itself, which reads as a bug.
        hydrate_read_state(&state, &mut read_state_hydrated).await;

        // A session ends by returning; the ladder and the pending publishes
        // survive it. Everything the connection owns is dropped here, so a
        // socket cannot leak across an iteration.
        session_loop(&state, conn, &creds, &mut commands, &mut pending).await;

        // Leaving `Connected` is what resets the ladder after a healthy run and
        // counts the reconnect (`Session::transition`), and `next_retry_delay`
        // then takes the rung — which also advances `attempt`, so the attempt
        // is read *after* the delay rather than before. Reading it first
        // reports the rung the previous outage used.
        //
        // One transition, not two. An earlier revision moved through
        // `Disconnected` on the way to `Reconnecting`, which incremented
        // `counters.reconnects` and then published a `disconnected` frame the
        // status bar would render for the width of one lock acquisition — a
        // flicker to the one state §2.6 says must stay distinct from the others.
        let (delay, attempt) = {
            let mut inner = state.lock().await;
            let delay = inner.session.next_retry_delay(false);
            (delay, inner.session.backoff_mut().attempt())
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

/// Install `ring` as the process-level rustls `CryptoProvider`.
///
/// Called from [`crate::state::AppState::new`] **and** from the top of [`run`].
/// Neither alone is sufficient, and the redundancy is the point:
///
/// - `main` alone is not enough. A library caller — which is exactly what
///   `tests/live_relay.rs` is — never runs it, and that is how this bug was
///   found: every live test that reached a `wss://` connect aborted with
///   `Could not automatically determine the process-level CryptoProvider`.
/// - [`run`] alone is not enough either. A **keyless** daemon never starts the
///   relay loop (§2.5 makes that a supported, visible state), so the HTTPS
///   bridge in [`crate::rest`] would still reach TLS with no provider.
///
/// `AppState::new` is the one constructor both paths pass through; [`run`]
/// keeps its own call so the loop is correct in isolation.
///
/// The failure it prevents is not subtle and not rare: `reqwest`'s rustls
/// feature pulls `aws-lc-rs` through `hyper-rustls`, `buzz-acp`/`buzz-dev-mcp`
/// pull `ring`, and a workspace build unifies both. With two providers enabled
/// rustls refuses to auto-select and panics at `ClientConfig::builder()`, which
/// for this crate is the NIP-42 handshake — so the daemon would abort on its
/// first connect, in production, with a message about crate features.
///
/// `let _ =` is deliberate: a second install returns `Err`, and a daemon
/// embedded in a process that already installed one (or a test binary running
/// several of these in sequence) must not treat that as a failure. The
/// post-condition is "a provider is installed", not "this call installed it".
pub(crate) fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
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

/// Seed the channel set from the relay's own membership answer (§4.1.1 d3).
///
/// **The cold-start walk.** Without it, a daemon that boots into an existing
/// community subscribes to nothing and stays that way: 44100/44101 are
/// *notifications of change*, so a community whose membership is settled emits
/// none, [`apply_membership_event`] never fires, and the channel registry stays
/// empty until a client happens to page history on a channel it cannot see in
/// the list. Measured against the live relay at M3: `connection.state` reached
/// `connected` while `GET /channel` returned `[]` and `buzz-cli` under the same
/// key at the same minute returned three channels.
///
/// Three properties this deliberately has:
///
/// - **It runs over the HTTP bridge, not the socket**, because the answer is a
///   bounded two-round-trip query and not a subscription. Asking for it as a
///   REQ would put 39002 and 39000 through the paced live path, where they
///   compete with the tails they exist to open.
/// - **It never blocks the loop.** A failed walk is logged and the session
///   proceeds on whatever set it already had; the live membership path still
///   works, and the next reconnect retries. A daemon that refused to run its
///   loop because a query 500'd would be strictly worse than one with a stale
///   channel list.
/// - **The lock is not held across either round trip.** `discover` takes
///   `&mut Channels`, so the cache is walked into a local and merged back under
///   a second, brief acquisition rather than awaiting HTTP under the mutex —
///   the same rule [`submit`] follows and for the same reason.
///
/// Returns the number of channels the walk registered, or `None` when it did
/// not run (keyless, or inside [`DISCOVERY_INTERVAL`] of the last one).
async fn hydrate_channels(state: &AppState, last: &mut Option<Instant>) -> Option<usize> {
    if let Some(at) = last {
        if at.elapsed() < DISCOVERY_INTERVAL {
            return None;
        }
    }
    let identity = state.identity_snapshot().await.ok()?;

    // Discovery runs against a detached cache so the two relay round trips
    // happen with no lock held. `Channels::discover` merges rosters and
    // metadata into whatever it is given, so an empty one yields exactly the
    // relay's answer and the merge below is what reconciles it with the ambient
    // state a running daemon has accumulated.
    let mut discovered = crate::channels::Channels::new();
    match discovered.discover(state.rest.as_ref(), &identity).await {
        Ok(count) => {
            *last = Some(Instant::now());
            let mut inner = state.lock().await;
            for channel in discovered.list() {
                // Subscribe *and* cache. Registering without caching leaves a
                // tail open for a channel the list cannot show; caching without
                // registering is the M2 symptom one layer down — a visible
                // channel whose timeline never fills.
                inner.session.subscriptions.subscribe(channel.id.clone());
                inner.channels.merge_preserving_ambient(channel);
            }
            for (channel_id, roster) in discovered.rosters() {
                inner
                    .channels
                    .set_roster(channel_id.clone(), roster.clone());
            }
            tracing::info!(channels = count, "cold-start channel discovery");
            Some(count)
        }
        Err(err) => {
            // Deliberately not fatal, and deliberately not a state transition:
            // the socket is fine, so reporting a connection problem here would
            // be the §1.3-property-3 failure in reverse — naming an outage that
            // is not happening.
            tracing::warn!(%err, "cold-start channel discovery failed; continuing on the cached set");
            None
        }
    }
}

/// The filter that reads this identity's own read-state slots back (kind 30078).
///
/// Author-scoped to self and kind-explicit, so it satisfies §2.4's invariant by
/// construction. `#d` is deliberately *not* constrained: slot ids rotate when a
/// squatter is detected (`ReadState::rotate_slot`), and a filter pinned to the
/// current id would silently miss the frontier written under the previous one.
pub fn read_state_filter(self_pubkey: &str) -> serde_json::Value {
    serde_json::json!({
        "kinds": [buzz_core::kind::KIND_READ_STATE],
        "authors": [self_pubkey],
        "limit": crate::readstate::MAX_SLOTS,
    })
}

/// Read this identity's read-state frontier back off the relay at cold start.
///
/// **Without this a restart resurrects every message as unread**, and the
/// second failure is worse than the first: the daemon *publishes* 30078 slots
/// but never *queried* them, so the first `mark` after a restart wrote a
/// frontier built from an empty map into the same `d` coordinate — overwriting
/// the multi-device frontier on the relay with a nearly-empty one. Read-state is
/// a CRDT whose merge is max-wins precisely so devices cannot rewind each other;
/// skipping the read turned this daemon into the device that could.
///
/// Runs once per process rather than per reconnect: after the first merge the
/// in-memory map is ahead of or equal to the relay's, and re-reading would
/// re-decrypt every slot to learn nothing. `merge` is max-wins, so a re-read
/// would be harmless — it is just waste.
///
/// Failure is not fatal, for the same reason [`hydrate_channels`]'s is not: a
/// daemon that refused to start its loop because one query failed is worse than
/// one whose unread counts are stale for a reconnect.
async fn hydrate_read_state(state: &AppState, done: &mut bool) -> Option<usize> {
    if *done {
        return None;
    }
    let identity = state.identity_snapshot().await.ok()?;
    let filter = read_state_filter(&identity.pubkey);
    let events = match state.rest.query(&identity, &filter).await {
        Ok(events) => events,
        Err(err) => {
            tracing::warn!(%err, "read-state hydration failed; unread counts start from empty");
            return None;
        }
    };

    // Decrypt outside the lock. NIP-44 over up to `MAX_SLOTS` blobs is real
    // work, and doing it under the daemon's single mutex would stall every
    // socket client for its duration — the rule `submit` and `hydrate_channels`
    // both follow.
    let keys = identity.signing_keys()?;
    let mut blobs = Vec::new();
    for event in &events {
        let Some(content) = event.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(plaintext) =
            nostr::nips::nip44::decrypt(keys.secret_key(), &keys.public_key(), content)
        else {
            // A slot this identity cannot decrypt is not this identity's slot.
            // Skipped rather than failed: one unreadable blob must not cost the
            // frontier carried by the others.
            continue;
        };
        if let Ok(blob) = serde_json::from_str::<crate::readstate::ReadStateBlob>(&plaintext) {
            blobs.push(blob);
        }
    }

    *done = true;
    let mut inner = state.lock().await;
    // Every decryptable blob is merged, including one written by a *different*
    // client id. That is not an oversight: max-wins is convergent precisely so
    // any device's markers can be folded in safely, and refusing another
    // client's frontier would reintroduce the rewind this function exists to
    // prevent. What slot ownership decides is where this daemon *writes*
    // (`slot_is_squatted` drives `rotate_slot` on the publish path), not what it
    // is allowed to read.
    let merged = blobs
        .iter()
        .filter(|blob| inner.read_state.merge(blob))
        .count();
    // **`merge` does not set `dirty`; only `mark` does** (`readstate.rs:197`
    // vs `:222`) — verified, because if it did, hydration would arm the
    // read-state debounce and a cold start would publish a frontier nobody
    // asked it to. That is a third write path, and the "exactly two" property
    // this module's header states does not admit one.
    tracing::info!(
        slots = events.len(),
        merged,
        contexts = inner.read_state.len(),
        "cold-start read-state hydration"
    );
    Some(merged)
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
    // When the relay's rate-limit gate is expected to disarm, if it is armed.
    let mut rate_limited_until: Option<Instant> = None;

    loop {
        // ── Read. The only await that can lose a frame, and it is never inside
        // a `select!`, so it is never cancelled.
        match conn.next_event(TICK).await {
            Ok(message) => {
                last_frame = Instant::now();
                probe_sent = None;
                match handle_message(state, message, pending).await {
                    Handled::Continue => {}
                    Handled::RateLimited { until } => rate_limited_until = Some(until),
                    Handled::EndSession => return,
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
        //
        // **No `continue` after sending.** An earlier revision had one, and
        // because `TICK == REQ_PACING_INTERVAL` the pacing condition is true on
        // essentially every tick while the queue is non-empty — so a resubscribe
        // of N channels meant N consecutive ticks that did nothing else. During
        // that window `drain_publishes` did not run (so a pending publish was
        // not merely delayed, its `PUBLISH_DEADLINE` went *unmeasured*), the
        // rate-limit gate could not disarm (exactly when a relay is most likely
        // to have armed it), and the liveness probe could not fire. The timer
        // above is what paces the REQs; skipping the rest of the body was never
        // part of that and only starved it.
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
            }
        }

        // ── One command per tick, for the same pacing reason.
        if let Ok(command) = commands.try_recv() {
            if !handle_command(state, command, pending, &mut queue).await {
                return;
            }
        }

        // ── Disarm the rate-limit gate once its window has passed.
        //
        // §2.7 gives the composer a live countdown, which is only honest if the
        // gate actually disarms when it reaches zero. A `NOTICE` arms it and
        // nothing on the relay side takes it back — there is no
        // "you-are-no-longer-rate-limited" frame in NIP-01 — so without this
        // the daemon stays `rate_limited` on a healthy socket forever, refusing
        // every write with a countdown that expired.
        if let Some(armed) = rate_limited_until {
            if Instant::now() >= armed {
                rate_limited_until = None;
                set_state(state, ConnectionState::Connected).await;
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

/// What the session loop should do after one relay frame.
///
/// An enum rather than a `bool` because a rate-limit `NOTICE` is a *third*
/// outcome: the session continues, but the loop now owns a deadline it has to
/// disarm. Encoding that as "continue" plus a side channel is how the gate ends
/// up armed forever on a healthy socket.
enum Handled {
    /// Nothing further; keep reading.
    Continue,
    /// The rate-limit gate is armed until this instant.
    RateLimited {
        /// When the gate is expected to disarm.
        until: Instant,
    },
    /// End the session; the caller reconnects.
    EndSession,
}

/// Handle one relay frame.
async fn handle_message(
    state: &AppState,
    message: RelayMessage,
    pending: &mut Vec<PendingPublish>,
) -> Handled {
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } => {
            // The probe's filter cannot match, so an event on it is a relay
            // behaving unexpectedly rather than data. Ignoring it keeps a
            // misbehaving relay from injecting into an unrouted store.
            if subscription_id == SUB_PROBE {
                return Handled::Continue;
            }
            let now = unix_now();
            let mut inner = state.lock().await;
            if let Ingested::Dropped(reason) =
                apply_relay_event(&mut inner, &subscription_id, &event, now)
            {
                tracing::debug!(reason, kind = kind_of(&event), "event dropped");
            }
            Handled::Continue
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
            //
            // **`retain`, not `iter_mut`.** `resolve` only takes the responder;
            // it does not remove the entry. An earlier revision iterated a
            // `&mut [PendingPublish]` — a slice, where removal is structurally
            // impossible — so an acknowledged publish answered its caller and
            // then stayed at the head of the queue, re-submitted over the
            // bridge every `PUBLISH_RETRY_INTERVAL` with nobody waiting on it:
            // a silent duplicate-write loop, and a third write path the "exactly
            // two" property does not admit.
            pending.retain_mut(|item| {
                if item.event.id.to_hex() != ok.event_id {
                    return true;
                }
                let local_id = item.local_id.clone();
                item.resolve(Ok(crate::post::SendResponse {
                    event_id: ok.event_id.clone(),
                    accepted: ok.accepted,
                    message: ok.message.clone(),
                    local_id,
                }));
                false
            });
            Handled::Continue
        }
        RelayMessage::Eose { .. } | RelayMessage::Count { .. } => Handled::Continue,
        RelayMessage::Closed {
            subscription_id,
            message,
        } => {
            // A `restricted:` close is a subscription that will never yield
            // rows — usually a `#p`-gated kind whose scope is wrong. Naming it
            // is what turns "the agent feed is empty" into a one-line answer.
            tracing::warn!(subscription = %subscription_id, %message, "relay closed a subscription");
            Handled::Continue
        }
        RelayMessage::Notice { message } => handle_notice(state, &message).await,
        // A challenge mid-session means the relay wants re-authentication,
        // which `connect_authenticated` performs at connect. Ending the session
        // and reconnecting is the honest handling; re-signing in place would
        // fork the auth path that already exists.
        RelayMessage::Auth { .. } => Handled::EndSession,
    }
}

/// Arm the rate-limit gate on a `rate-limited:` notice (§2.7).
async fn handle_notice(state: &AppState, message: &str) -> Handled {
    let lower = message.to_lowercase();
    if !lower.contains("rate-limit") && !lower.contains("rate limited") {
        tracing::info!(notice = %message, "relay notice");
        return Handled::Continue;
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
    Handled::RateLimited {
        until: Instant::now() + Duration::from_millis(retry_after_ms),
    }
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
                last_attempt: None,
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
/// One per tick, and **not on every tick**: a publish is an HTTP round trip,
/// and a failing one retried at [`TICK`] would fire ~240 requests at the relay
/// across a single [`PUBLISH_DEADLINE`] — a retry storm aimed at a relay that
/// is already unwell. [`PUBLISH_RETRY_INTERVAL`] paces it.
async fn drain_publishes(state: &AppState, pending: &mut Vec<PendingPublish>) {
    // **Unconditionally, first.** An earlier revision only reached
    // `fail_expired` from this function's failure arm, which meant a publish
    // queued *behind* a succeeding one aged past `PUBLISH_DEADLINE` with
    // nothing ever checking it: `WireHandle::publish` awaits its oneshot bare
    // and there is no timeout layer above it, so the client's
    // `POST /channel/{id}/message` hung forever with the composed text stuck.
    // That is precisely the ambiguous outcome §2.7 exists to prevent, reached
    // by way of the queue rather than the relay. The deadline has to be swept
    // on every tick, not only when the head fails.
    fail_expired(pending);
    let Some(item) = pending.first_mut() else {
        return;
    };
    if item
        .last_attempt
        .is_some_and(|at| at.elapsed() < PUBLISH_RETRY_INTERVAL)
    {
        return;
    }
    item.last_attempt = Some(Instant::now());
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
    // The identity is **cloned out** and the lock released before the request.
    // Awaiting an HTTP round trip under the daemon's single mutex would stall
    // every other socket client for its duration — including an `/event` reader
    // whose whole job is to be prompt — and a relay that is timing out is
    // exactly when that matters most. The clone carries the same zeroizing
    // buffer and hand-written `Debug`, so §2.5's controls hold per copy.
    let identity = state.identity_snapshot().await.ok()?;
    let outcome = state.rest.submit_event(&identity, event).await;
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
            last_attempt: None,
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
            last_attempt: None,
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
    /// §2.7: a publish is retried on a **paced** interval, not once per tick.
    /// Without the pace, a failing publish fires ~240 HTTP requests across one
    /// deadline, at a relay that is by definition already unwell.
    #[tokio::test]
    async fn a_failing_publish_is_not_retried_every_tick() {
        assert!(
            PUBLISH_RETRY_INTERVAL > TICK * 4,
            "the retry interval must be meaningfully slower than the loop tick"
        );
        // Attempts per deadline, at the paced rate. The tick-rate figure is
        // `PUBLISH_DEADLINE / TICK` = 240, which is the number this bounds.
        let attempts = PUBLISH_DEADLINE.as_millis() / PUBLISH_RETRY_INTERVAL.as_millis();
        assert!(
            attempts <= 20,
            "{attempts} attempts per deadline is a retry storm"
        );
    }

    /// A rate-limit `NOTICE` arms the gate with a deadline the loop can disarm.
    ///
    /// There is no "you-are-no-longer-rate-limited" frame in NIP-01, so nothing
    /// on the relay side takes the gate back. Returning the deadline from
    /// `handle_notice` is what lets the session loop restore `connected` when
    /// the countdown the composer is showing actually reaches zero — otherwise
    /// the daemon stays `rate_limited` on a healthy socket forever, refusing
    /// every write against an expired countdown.
    #[tokio::test]
    async fn a_rate_limit_notice_carries_a_deadline_the_loop_can_disarm() {
        let state = AppState::new(config(), None).expect("state");
        // The relay's hint grammar is `retry in <n>s` — `parse_retry_hint`
        // requires the bare `s` suffix (`rest.rs:145`). Spelled out here
        // because "retry in 3 seconds" parses to `None` and falls back to the
        // 1 s default, which is a countdown that is simply wrong rather than a
        // visible failure.
        let armed = handle_notice(&state, "rate-limited: retry in 3s").await;
        let Handled::RateLimited { until } = armed else {
            panic!("a rate-limit notice must arm the gate with a deadline");
        };
        let remaining = until.saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_secs(3) && remaining > Duration::from_secs(2),
            "expected ~3s from the relay's hint, got {remaining:?}"
        );
        assert!(matches!(
            state.lock().await.session.state(),
            ConnectionState::RateLimited { .. }
        ));
    }

    /// A rate-limit notice with no parseable hint still arms a **bounded**
    /// gate. An unbounded one is the same forever-armed bug by another route:
    /// the composer would show a countdown that never reaches zero.
    #[tokio::test]
    async fn a_hintless_rate_limit_notice_still_arms_a_bounded_gate() {
        let state = AppState::new(config(), None).expect("state");
        let armed = handle_notice(&state, "rate-limited: slow down").await;
        let Handled::RateLimited { until } = armed else {
            panic!("a hintless rate-limit notice must still arm the gate");
        };
        assert!(until.saturating_duration_since(Instant::now()) <= Duration::from_secs(2));
    }

    /// A relay-supplied hint is **capped**. An uncapped one is a
    /// relay-controlled hang: a hostile or buggy relay could park every write
    /// behind an hour-long countdown.
    #[tokio::test]
    async fn a_relay_hint_cannot_park_writes_indefinitely() {
        let state = AppState::new(config(), None).expect("state");
        let armed = handle_notice(&state, "rate-limited: retry in 99999s").await;
        let Handled::RateLimited { until } = armed else {
            panic!("expected the gate to arm");
        };
        assert!(
            until.saturating_duration_since(Instant::now())
                <= Duration::from_secs(crate::rest::RETRY_IN_MAX_SECS),
            "the hint must be capped at RETRY_IN_MAX_SECS"
        );
    }

    /// An ordinary notice is not a gate. Arming on every `NOTICE` would refuse
    /// writes because the relay said something conversational.
    #[tokio::test]
    async fn an_ordinary_notice_does_not_arm_the_gate() {
        let state = AppState::new(config(), None).expect("state");
        assert!(matches!(
            handle_notice(&state, "restricted: not a member").await,
            Handled::Continue
        ));
        assert!(matches!(
            state.lock().await.session.state(),
            ConnectionState::Disconnected
        ));
    }
    /// **W1 regression.** A publish queued behind a succeeding one must still
    /// hit its deadline. `fail_expired` used to run only from
    /// `drain_publishes`'s failure arm, so an entry that was never at the head
    /// during a failure aged forever — and `WireHandle::publish` awaits its
    /// oneshot bare, with no timeout layer above it, so the client's POST hung
    /// with the composed text stuck. Silence is the one outcome §2.7 forbids.
    #[tokio::test]
    async fn a_publish_behind_a_healthy_one_still_hits_its_deadline() {
        let keys = Keys::generate();
        let event = |n: u8| {
            EventBuilder::new(Kind::Custom(9), format!("m{n}"))
                .sign_with_keys(&keys)
                .expect("sign")
        };
        let (head_tx, _head_rx) = tokio::sync::oneshot::channel();
        let (tail_tx, tail_rx) = tokio::sync::oneshot::channel();
        let mut pending = vec![
            PendingPublish {
                event: event(1),
                respond: Some(head_tx),
                local_id: None,
                asked_at: Instant::now(),
                last_attempt: None,
            },
            PendingPublish {
                event: event(2),
                respond: Some(tail_tx),
                local_id: None,
                // Queued long ago and never at the head.
                asked_at: Instant::now() - PUBLISH_DEADLINE - Duration::from_secs(1),
                last_attempt: None,
            },
        ];

        // The sweep the loop performs at the top of every `drain_publishes`.
        fail_expired(&mut pending);

        assert_eq!(pending.len(), 1, "the expired tail must be dropped");
        assert!(matches!(
            tail_rx
                .await
                .expect("the caller is answered, never dropped"),
            Err(DaemonError::RelayUnreachable)
        ));
    }

    /// **W2 regression.** An `OK` over the socket must *remove* the pending
    /// entry, not merely answer its caller. Resolving without removing left the
    /// event at the head of the queue, re-submitted over the bridge every
    /// `PUBLISH_RETRY_INTERVAL` with nobody waiting: a silent duplicate-write
    /// loop, and a third write path the "exactly two" property does not admit.
    #[tokio::test]
    async fn an_ok_removes_the_pending_publish_rather_than_only_answering_it() {
        let state = AppState::new(config(), None).expect("state");
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), "sent")
            .sign_with_keys(&keys)
            .expect("sign");
        let event_id = event.id.to_hex();
        let (respond, wait) = tokio::sync::oneshot::channel();
        let mut pending = vec![PendingPublish {
            event,
            respond: Some(respond),
            local_id: Some("local-9".into()),
            asked_at: Instant::now(),
            last_attempt: None,
        }];

        let ok = buzz_ws_client::OkResponse {
            event_id: event_id.clone(),
            accepted: true,
            message: String::new(),
        };
        handle_message(&state, RelayMessage::Ok(ok), &mut pending).await;

        assert!(
            pending.is_empty(),
            "an acknowledged publish must leave the queue, or it is re-sent forever"
        );
        let response = wait.await.expect("answered").expect("accepted");
        assert_eq!(response.event_id, event_id);
        assert_eq!(response.local_id.as_deref(), Some("local-9"));
    }

    /// **W3 regression.** A dropped event must leave the watermark where it
    /// was. Advancing it before the routing decision meant the reconnect replay
    /// began *after* an event that was deduped and never delivered — permanent
    /// invisibility, reached by the watermark rather than the dedup set.
    #[tokio::test]
    async fn a_dropped_event_moves_neither_the_dedup_set_nor_the_watermark() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        inner.session.subscriptions.subscribe(CHANNEL);
        let author = Keys::generate();
        // Routed to a subscription that exists, but dropped by the handler: an
        // observer frame at a keyless daemon.
        let frame = EventBuilder::new(Kind::Custom(24_200), "x")
            .tags([Tag::parse(["h", CHANNEL]).unwrap()])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&author)
            .expect("sign");

        let outcome = apply_relay_event(&mut inner, SUB_OBSERVER, &frame, NOW);
        assert!(matches!(outcome, Ingested::Dropped(_)), "{outcome:?}");
        assert!(
            !inner.session.seen.contains(&frame.id.to_hex()),
            "a dropped event must be un-deduped so a resubscribe can re-deliver it"
        );
        assert_eq!(
            inner.session.subscriptions.resubscribe_since(CHANNEL),
            Some(0),
            "the watermark must not have advanced past an undelivered event"
        );
    }

    /// The other half of W3: a **delivered** event does advance the watermark.
    /// The fix must not trade one failure for the opposite one.
    #[tokio::test]
    async fn a_delivered_event_still_advances_the_watermark() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        inner.session.subscriptions.subscribe(CHANNEL);
        let author = Keys::generate();
        let message = channel_message(&author, CHANNEL, "hello", vec![], NOW);

        let outcome = apply_relay_event(&mut inner, &channel_sub_id(CHANNEL), &message, NOW);
        assert!(outcome.topics().contains(&"message.new".to_string()));
        assert_eq!(
            inner.session.subscriptions.resubscribe_since(CHANNEL),
            Some(NOW as u64 - crate::session::SINCE_SKEW_SECS)
        );
    }

    /// **M3 regression.** Read-state hydration must not arm the publish
    /// debounce.
    ///
    /// The gate on the second write path is `ReadState::is_dirty`, and the
    /// module header's "exactly two write paths" property rests on nothing else
    /// setting it. `merge` does not (`readstate.rs:197` sets no flag; only
    /// `mark` at `:222` does) — but that is a property of another module, so it
    /// is pinned here rather than assumed. If it ever changed, a cold start
    /// would publish a frontier nobody asked it to, over the live relay, on
    /// every daemon launch.
    #[tokio::test]
    async fn merging_a_frontier_does_not_arm_the_publish_debounce() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        assert!(!inner.read_state.is_dirty());

        let blob = crate::readstate::ReadStateBlob {
            v: 1,
            client_id: "another-device".into(),
            contexts: [("channel:abc".to_string(), 1_785_000_000_u64)]
                .into_iter()
                .collect(),
        };
        assert!(inner.read_state.merge(&blob), "the frontier moved");
        assert!(
            !inner.read_state.is_dirty(),
            "hydration is not a local edit; arming the debounce here would make \
             a cold start publish, which is a third write path"
        );
    }

    /// The read-state filter must be scoped to self and must not pin `#d`.
    ///
    /// Slot ids rotate when a squatter is detected
    /// (`ReadState::rotate_slot`), so a filter pinned to the *current* id
    /// silently misses the frontier written under the previous one — and the
    /// symptom is indistinguishable from having no frontier at all.
    #[tokio::test]
    async fn the_read_state_filter_is_self_scoped_and_slot_agnostic() {
        let me = "aa".repeat(32);
        let filter = read_state_filter(&me);
        crate::search::assert_explicit_kinds(&filter, "read-state").expect("explicit kinds");
        assert_eq!(filter["authors"], serde_json::json!([me]));
        assert!(
            filter.get("#d").is_none(),
            "pinning the slot id would miss a rotated slot's frontier"
        );
    }

    /// **M3 regression.** A membership event must not move a *channel's*
    /// timeline watermark.
    ///
    /// 44100/44101 carry an `h` tag naming the channel
    /// (`buzz-relay/src/handlers/side_effects.rs:914`), and the watermark key
    /// used to be that tag. So a join advanced the channel's `since` to the
    /// join's own `created_at` — and since the same call had just registered
    /// the channel at `last_seen: 0`, the very next `plan_subscriptions` asked
    /// for `since: now - SKEW` instead of the full `CHANNEL_TAIL_LIMIT` tail.
    /// You joined a channel and its timeline was empty, silently.
    #[tokio::test]
    async fn a_join_does_not_advance_the_channels_timeline_watermark() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;

        let author = Keys::generate();
        let joined = EventBuilder::new(Kind::Custom(44_100), "")
            .tags([
                Tag::parse(["h", CHANNEL]).unwrap(),
                Tag::public_key(author.public_key()),
            ])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&author)
            .expect("sign");
        apply_relay_event(&mut inner, SUB_MEMBERSHIP, &joined, NOW);

        assert_eq!(
            inner.session.subscriptions.resubscribe_since(CHANNEL),
            Some(0),
            "a membership notification is not channel traffic; the tail must \
             open with no `since` so CHANNEL_TAIL_LIMIT is what bounds it"
        );
        // The membership subscription's own replay window *does* advance —
        // that half was always right and must not regress with the fix.
        assert_eq!(
            inner.session.subscriptions.membership_replay_since(None),
            Some(NOW as u64)
        );
    }

    /// **M3 regression.** Being removed from a channel must drop its tail, not
    /// register one.
    ///
    /// The membership filter is `{kinds:[44100,44101], #p:[self]}` and both
    /// kinds landed in the same unconditional `subscribe`. So a removal
    /// *registered* a permanent live tail on a channel the relay would answer
    /// `CLOSED restricted:` to on every reconnect forever — `subscribe` is an
    /// `or_insert` with no cap and `unsubscribe` had no production caller.
    #[tokio::test]
    async fn a_removal_unsubscribes_rather_than_subscribing() {
        let me = Keys::generate();
        let identity = crate::identity::Identity::new(
            me.public_key().to_hex(),
            zeroize::Zeroizing::new(me.secret_key().to_secret_bytes().to_vec()),
            None,
        );
        let state = AppState::new(config(), Some(identity)).expect("state");
        let mut inner = state.lock().await;

        let relay = Keys::generate();
        let removed = EventBuilder::new(Kind::Custom(44_101), "")
            .tags([
                Tag::parse(["h", CHANNEL]).unwrap(),
                Tag::public_key(me.public_key()),
            ])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&relay)
            .expect("sign");

        inner.session.subscriptions.subscribe(CHANNEL);
        apply_relay_event(&mut inner, SUB_MEMBERSHIP, &removed, NOW);
        assert!(
            inner
                .session
                .subscriptions
                .resubscribe_since(CHANNEL)
                .is_none(),
            "our own removal must drop the tail"
        );

        // A *peer's* removal from a channel we are still in must not touch it.
        // Dropping the tail there would be a silent blackout of a live channel.
        let peer = Keys::generate();
        let peer_left = EventBuilder::new(Kind::Custom(44_101), "")
            .tags([
                Tag::parse(["h", CHANNEL]).unwrap(),
                Tag::public_key(peer.public_key()),
            ])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64 + 1))
            .sign_with_keys(&relay)
            .expect("sign");
        inner.session.subscriptions.subscribe(CHANNEL);
        apply_relay_event(&mut inner, SUB_MEMBERSHIP, &peer_left, NOW);
        assert!(
            inner
                .session
                .subscriptions
                .resubscribe_since(CHANNEL)
                .is_some(),
            "somebody else leaving must not close our tail"
        );
    }

    /// **M3 regression, the pacing half.** A flapping socket must not turn the
    /// cold-start walk into a discovery storm.
    ///
    /// The walk is two HTTP round trips. Running it on every reconnect aims that
    /// pair at a relay which is by definition already unwell, and buys nothing:
    /// once the registry is seeded the loop maintains it live from 44100/44101,
    /// so a reconnect seconds later reads the same answer.
    ///
    /// Asserted at the gate rather than end to end, because the round trips
    /// themselves need a relay. `None` is the "did not run" answer, and a
    /// just-walked timestamp must produce it. The *other* direction — that a
    /// never-walked daemon walks immediately — is what
    /// `a_cold_daemon_discovers_its_channels_without_being_seeded` proves live;
    /// a `None` here with `last = None` would mean the daemon never discovers at
    /// all, which is the defect this lane fixed.
    #[tokio::test]
    async fn a_reconnect_storm_does_not_become_a_discovery_storm() {
        let state = AppState::new(config(), None).expect("state");
        let mut last = Some(Instant::now());
        assert!(
            hydrate_channels(&state, &mut last).await.is_none(),
            "a walk inside DISCOVERY_INTERVAL of the last one must not run"
        );

        // And a keyless daemon never walks, whatever the clock says: the walk is
        // an authenticated query, so without an identity there is nothing to
        // sign it with. It must decline rather than error the loop out.
        let mut never = None;
        assert!(
            hydrate_channels(&state, &mut never).await.is_none(),
            "a keyless daemon has no identity to discover with"
        );
        assert!(
            never.is_none(),
            "a walk that did not happen must not stamp the clock, or the first \
             walk after a key arrives would be suppressed for a full interval"
        );
    }

    /// **W4 regression.** Membership discovery must register the channel in the
    /// **subscription registry**, not only in the channel cache. Updating only
    /// the cache meant a fresh daemon subscribed to zero of its channels until a
    /// client paged history on each — the same permanently-empty-stores symptom
    /// this module exists to fix.
    #[tokio::test]
    async fn membership_discovery_registers_the_channel_for_subscription() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        assert!(inner.session.subscriptions.is_empty());

        let author = Keys::generate();
        let joined = EventBuilder::new(Kind::Custom(44_100), "")
            .tags([
                Tag::parse(["h", CHANNEL]).unwrap(),
                Tag::public_key(author.public_key()),
            ])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&author)
            .expect("sign");

        apply_relay_event(&mut inner, SUB_MEMBERSHIP, &joined, NOW);
        assert_eq!(inner.session.subscriptions.len(), 1);
        let planned = plan_subscriptions(&inner, &"bb".repeat(32), NOW);
        assert!(
            planned
                .iter()
                .any(|req| req.subscription_id == channel_sub_id(CHANNEL)),
            "the discovered channel must appear in the next resubscribe plan"
        );
    }

    /// A membership event naming something that is not a uuid registers
    /// nothing. `Subscriptions::subscribe` is an `or_insert` with no cap and no
    /// production `unsubscribe`, so an unvalidated id is a permanent entry that
    /// costs a paced REQ on every reconnect forever.
    #[tokio::test]
    async fn a_membership_event_with_a_junk_channel_id_registers_nothing() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let author = Keys::generate();
        let junk = EventBuilder::new(Kind::Custom(44_100), "")
            .tags([
                Tag::parse(["h", "../../etc/passwd"]).unwrap(),
                Tag::public_key(author.public_key()),
            ])
            .custom_created_at(nostr::Timestamp::from_secs(NOW as u64))
            .sign_with_keys(&author)
            .expect("sign");

        apply_relay_event(&mut inner, SUB_MEMBERSHIP, &junk, NOW);
        assert!(inner.session.subscriptions.is_empty());
    }

    /// **W5 regression.** Staleness is measured against the **daemon's** clock.
    /// Passing the event's own `created_at` as `now` makes `now - last_seen`
    /// zero by construction, so the beat TTL can never elapse and any peer pins
    /// itself `present` forever — the looks-alive-while-it-is-dead failure
    /// §1.3 property 3 forbids, in the one store whose job is not to make that
    /// claim.
    #[tokio::test]
    async fn a_stale_beat_does_not_pin_an_agent_present() {
        let state = AppState::new(config(), None).expect("state");
        let mut inner = state.lock().await;
        let agent = Keys::generate();
        let long_ago = NOW - crate::presence::PRESENCE_BEAT_TTL_SECS - 60;
        let beat = EventBuilder::new(Kind::Custom(20_001), "")
            .tags([Tag::parse(["status", "online"]).unwrap()])
            .custom_created_at(nostr::Timestamp::from_secs(long_ago as u64))
            .sign_with_keys(&agent)
            .expect("sign");

        // Ingested with the daemon's clock at NOW, not the event's.
        apply_relay_event(&mut inner, SUB_PRESENCE, &beat, NOW);
        assert_eq!(
            inner.presence.get(&agent.public_key().to_hex(), NOW).state,
            crate::presence::Presence::Unknown,
            "a beat older than the TTL must lapse to unknown, not stay present"
        );
    }
}
