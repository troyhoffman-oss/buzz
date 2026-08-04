//! `GET /event` — the one event stream, and its per-topic drop policy.
//!
//! Implements `DESIGN.md` §2.4 [D-5], §2.6 (link A), and Wave-1 daemon
//! deliverable 13 (§4.1.1): ndjson by default, SSE by content negotiation, a
//! daemon-global monotonic `seq`, a 10k ring, `?since=` replay, and
//! `stream.reset` when a cursor has aged out.
//!
//! # [D-5] Per-topic drop policy, not disconnect-on-overflow
//!
//! `daemon-api.md` proposes killing a slow consumer. That is honest but wrong
//! for a TUI that legitimately blocks for a second while rendering a large
//! diff. The stream inherits the harness's ephemeral-vs-durable split instead:
//!
//! - **Durable** topics are never dropped. They park in order. If the park
//!   queue exceeds its cap, the daemon emits `stream.overflow{dropped,
//!   since_seq}` and *then* disconnects — **loss is announced before it
//!   happens**, which is §1.3 property 3 applied to the daemon→TUI hop.
//! - **Coalescing** topics are latest-wins per key. Dropping an intermediate
//!   value is semantically free because the next one supersedes it.
//!
//! This is the same distinction `crates/buzz-acp/src/relay.rs` already makes
//! between typing (dropped under the rate-limit gate) and observer telemetry
//! (parked and paced). Making the daemon→TUI hop obey the same rule means one
//! mental model end to end.

use serde::{Deserialize, Serialize};

/// Capacity of the replay ring backing `?since=` (§2.6 link A).
///
/// When a client's cursor has aged out of this ring the daemon sends
/// `stream.reset` **first**, and the TUI invalidates and re-fetches rather than
/// presenting a silently gapped timeline.
pub const EVENT_RING_CAPACITY: usize = 10_000;

/// How a topic behaves when a client's buffer backs up ([D-5]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropPolicy {
    /// Never dropped; parked in order. Overflow announces
    /// `stream.overflow{dropped, since_seq}` and then disconnects.
    Durable,
    /// Latest-wins per key; intermediate values are semantically free to drop.
    Coalescing,
}

/// The durable topics of [D-5]'s table.
pub const DURABLE_TOPICS: &[&str] = &[
    "message.new",
    "message.update",
    "message.delete",
    "thread.reply",
    "agent.frame",
    "agent.metric",
    "agent.permission.request",
    "read_state.update",
    "channel.member",
    "connection.state",
];

/// The coalescing topics of [D-5]'s table.
pub const COALESCING_TOPICS: &[&str] = &[
    "presence.update",
    "typing.start",
    "channel.unread",
    "agent.state",
];

/// Resolve a topic's drop policy.
///
/// Durable is matched by prefix for the `message.*` family, which [D-5]'s table
/// writes with a wildcard. Anything unrecognized defaults to [`DropPolicy::Durable`]:
/// a new topic that has not been classified must not silently become droppable.
pub fn policy_for(topic: &str) -> DropPolicy {
    if COALESCING_TOPICS.contains(&topic) {
        DropPolicy::Coalescing
    } else {
        DropPolicy::Durable
    }
}

/// A frame on the stream. `seq` is **daemon-global and monotonic**, which is
/// what makes `?since=<seq>` a total order rather than a per-topic one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamFrame {
    /// Daemon-global monotonic sequence number.
    pub seq: u64,
    /// Topic name, e.g. `message.new`.
    #[serde(rename = "type")]
    pub topic: String,
    /// Topic-specific payload.
    pub payload: serde_json::Value,
}

/// Control frames the daemon emits on its own behalf (§2.6, [D-5]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamControl {
    /// The requested `since` cursor has aged out of the ring. Sent **before**
    /// any further data so the TUI invalidates and re-fetches rather than
    /// presenting a silently gapped timeline.
    #[serde(rename = "stream.reset")]
    Reset,
    /// A durable-topic park queue overflowed. Announced **before** the
    /// disconnect that follows, so loss is never silent.
    #[serde(rename = "stream.overflow")]
    Overflow {
        /// How many frames were lost.
        dropped: u64,
        /// The last sequence number the client is known to have received.
        since_seq: u64,
    },
}

/// Wire encoding of the stream, chosen by content negotiation.
///
/// ndjson is the default for the TUI: simpler to parse, no `data:` framing, and
/// no reconnect semantics baked into the transport that the daemon's own
/// `?since=` cursor would then have to fight. SSE is offered because browsers
/// and `curl` want it, and it costs one branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// `application/x-ndjson` — one JSON object per line.
    Ndjson,
    /// `text/event-stream` — SSE framing.
    Sse,
}

impl Encoding {
    /// Negotiate from an `Accept` header.
    ///
    /// ndjson wins ties and absence. The check is a substring match rather than
    /// a full media-type parse because the only question being asked is "did
    /// this client specifically ask for SSE" — a browser sends
    /// `text/event-stream` verbatim, and anything else gets the default.
    pub fn negotiate(accept: Option<&str>) -> Self {
        match accept {
            Some(value) if value.contains("text/event-stream") => Self::Sse,
            _ => Self::Ndjson,
        }
    }

    /// The `Content-Type` this encoding responds with.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Ndjson => "application/x-ndjson",
            Self::Sse => "text/event-stream",
        }
    }

    /// Encode one frame, including its trailing delimiter.
    pub fn encode(self, frame: &StreamFrame) -> String {
        let json = serde_json::to_string(frame).unwrap_or_else(|_| "{}".into());
        match self {
            Self::Ndjson => format!("{json}\n"),
            // `id:` carries the same `seq` the ndjson body does, so an SSE
            // client's own `Last-Event-ID` reconnect and the daemon's `?since=`
            // cursor are the same number rather than two schemes to reconcile.
            Self::Sse => format!("id: {}\ndata: {json}\n\n", frame.seq),
        }
    }
}

/// The result of a client's `?since=` request.
#[derive(Debug, Clone, PartialEq)]
pub enum Replay {
    /// Frames from the ring, in order.
    Frames(Vec<StreamFrame>),
    /// The cursor has aged out. The client receives [`StreamControl::Reset`]
    /// **first** and must invalidate and re-fetch.
    Reset,
}

/// The event-stream fan-out: the ring, the sequence, and `?since=` replay.
///
/// Implements §4.1.1 deliverable 13 and §2.6's link-A behaviour.
#[derive(Debug)]
pub struct EventStream {
    next_seq: u64,
    ring: std::collections::VecDeque<StreamFrame>,
    capacity: usize,
    /// Latest value per coalescing key, so a slow client gets the current value
    /// rather than every intermediate one ([D-5]).
    coalesced: std::collections::BTreeMap<String, StreamFrame>,
}

impl Default for EventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStream {
    /// A stream whose first frame will carry `seq = 1`.
    pub fn new() -> Self {
        Self::with_capacity(EVENT_RING_CAPACITY)
    }

    /// A stream with an explicit ring capacity, for tests.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            next_seq: 0,
            ring: std::collections::VecDeque::with_capacity(capacity.min(1024)),
            capacity,
            coalesced: std::collections::BTreeMap::new(),
        }
    }

    /// Allocate the next daemon-global sequence number.
    pub fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Publish a frame, assigning it a sequence number.
    ///
    /// Coalescing topics ([D-5]) replace their previous value **in place** and
    /// keep their original sequence position, so the ring stays monotonic and a
    /// `?since=` replay does not deliver a presence beat as if it were new.
    /// Latest-wins is semantically free for these topics precisely because the
    /// next value supersedes the last.
    pub fn publish(&mut self, topic: impl Into<String>, payload: serde_json::Value) -> u64 {
        let topic = topic.into();
        if policy_for(&topic) == DropPolicy::Coalescing {
            if let Some(key) = coalescing_key(&topic, &payload) {
                if let Some(existing) = self.coalesced.get(&key) {
                    let seq = existing.seq;
                    let frame = StreamFrame {
                        seq,
                        topic: topic.clone(),
                        payload: payload.clone(),
                    };
                    if let Some(slot) = self.ring.iter_mut().find(|f| f.seq == seq) {
                        *slot = frame.clone();
                        self.coalesced.insert(key, frame);
                        return seq;
                    }
                    // The superseded frame has already aged out of the ring, so
                    // this value is genuinely new to any client still reading.
                    self.coalesced.remove(&key);
                }
                let seq = self.next_seq();
                let frame = StreamFrame {
                    seq,
                    topic,
                    payload,
                };
                self.coalesced.insert(key, frame.clone());
                self.push(frame);
                return seq;
            }
        }
        let seq = self.next_seq();
        self.push(StreamFrame {
            seq,
            topic,
            payload,
        });
        seq
    }

    fn push(&mut self, frame: StreamFrame) {
        if self.ring.len() >= self.capacity {
            if let Some(evicted) = self.ring.pop_front() {
                // Keep the coalescing index from pointing at a frame the ring
                // no longer holds; a stale entry there would make `publish`
                // try to overwrite a sequence number that has aged out.
                self.coalesced.retain(|_, held| held.seq != evicted.seq);
            }
        }
        self.ring.push_back(frame);
    }

    /// Replay everything after `since`, or [`Replay::Reset`] when the cursor has
    /// aged out of the ring (§2.6).
    ///
    /// "If the cursor has aged out, the daemon sends `stream.reset` **first**,
    /// and the TUI invalidates and re-fetches rather than presenting a silently
    /// gapped timeline." Detecting it requires comparing against the *oldest*
    /// frame still held, not against the ring's length — a ring that has never
    /// filled has evicted nothing, and a cursor below its floor is a cursor from
    /// a previous daemon process.
    pub fn replay(&self, since: u64) -> Replay {
        // A client that has seen everything is caught up, not reset — the
        // common case of a reconnect that lost nothing.
        if since == self.next_seq {
            return Replay::Frames(Vec::new());
        }
        // A cursor *ahead* of us is from a previous daemon process whose
        // sequence started over. Resetting is the honest answer: the numbers do
        // not refer to the same events.
        if since > self.next_seq {
            return Replay::Reset;
        }
        let floor = self.ring.front().map(|f| f.seq);
        match floor {
            // `since + 1` is the first frame the client has not seen. If the
            // ring's oldest frame is newer than that, the gap is real.
            Some(oldest) if oldest > since + 1 => Replay::Reset,
            None if since > 0 => Replay::Reset,
            _ => Replay::Frames(
                self.ring
                    .iter()
                    .filter(|frame| frame.seq > since)
                    .cloned()
                    .collect(),
            ),
        }
    }

    /// The newest sequence number issued.
    pub fn latest_seq(&self) -> u64 {
        self.next_seq
    }

    /// How many frames the ring currently holds.
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether nothing has been published.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// The coalescing key for a topic's payload ([D-5]: "latest-wins **per key**").
///
/// Per-key, not per-topic: coalescing every `presence.update` onto one slot
/// would mean one agent's beat erasing another's. The key is whatever
/// identifies the thing the frame is *about*.
///
/// A payload with no identifiable subject returns `None` and is treated as
/// durable — an unclassifiable frame must not silently become droppable, the
/// same rule [`policy_for`] applies to an unknown topic.
fn coalescing_key(topic: &str, payload: &serde_json::Value) -> Option<String> {
    let field = |name: &str| {
        payload
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let subject = match topic {
        "presence.update" | "agent.state" => field("pubkey"),
        "channel.unread" => field("channel_id"),
        "typing.start" => {
            let channel = field("channel_id")?;
            let pubkey = field("pubkey")?;
            Some(format!("{channel}/{pubkey}"))
        }
        _ => None,
    }?;
    Some(format!("{topic}:{subject}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [D-5]: durable topics are never dropped.
    #[test]
    fn durable_topics_are_durable() {
        for topic in DURABLE_TOPICS {
            assert_eq!(policy_for(topic), DropPolicy::Durable, "{topic}");
        }
    }

    /// [D-5]: coalescing topics are latest-wins per key.
    #[test]
    fn coalescing_topics_coalesce() {
        for topic in COALESCING_TOPICS {
            assert_eq!(policy_for(topic), DropPolicy::Coalescing, "{topic}");
        }
    }

    /// An unclassified topic must not silently become droppable.
    #[test]
    fn unknown_topics_default_to_durable() {
        assert_eq!(policy_for("something.new"), DropPolicy::Durable);
    }

    /// The two sets must not overlap — a topic with two policies is a bug that
    /// would resolve by accident of match order.
    #[test]
    fn topic_classes_are_disjoint() {
        for topic in DURABLE_TOPICS {
            assert!(
                !COALESCING_TOPICS.contains(topic),
                "{topic} is in both classes"
            );
        }
    }

    /// §2.6: `seq` is daemon-global and monotonic, which is what makes
    /// `?since=` a total order.
    #[test]
    fn sequence_numbers_are_monotonic_from_one() {
        let mut stream = EventStream::new();
        assert_eq!(stream.next_seq(), 1);
        assert_eq!(stream.next_seq(), 2);
        assert_eq!(stream.next_seq(), 3);
    }

    /// §2.6: `stream.reset` and `stream.overflow` are the announced-loss
    /// frames, so their wire names are contract.
    #[test]
    fn control_frames_use_their_wire_names() {
        let reset = serde_json::to_string(&StreamControl::Reset).unwrap();
        assert!(reset.contains("stream.reset"), "{reset}");
        let overflow = serde_json::to_string(&StreamControl::Overflow {
            dropped: 12,
            since_seq: 900,
        })
        .unwrap();
        assert!(overflow.contains("stream.overflow"), "{overflow}");
        assert!(overflow.contains("\"dropped\":12"), "{overflow}");
    }

    #[test]
    fn ring_capacity_matches_the_design() {
        assert_eq!(EVENT_RING_CAPACITY, 10_000);
    }

    // ── The ring and `?since=` replay (§2.6 link A) ───────────────────────

    fn message(n: u64) -> serde_json::Value {
        serde_json::json!({"id": format!("event-{n}")})
    }

    #[test]
    fn replay_returns_everything_after_the_cursor() {
        let mut stream = EventStream::new();
        for n in 1..=5 {
            stream.publish("message.new", message(n));
        }
        let Replay::Frames(frames) = stream.replay(2) else {
            panic!("a live cursor replays, it does not reset");
        };
        let seqs: Vec<u64> = frames.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, [3, 4, 5]);
    }

    /// A client that has seen everything is **caught up**, not reset — the
    /// common case of a reconnect that lost nothing.
    #[test]
    fn a_fully_caught_up_cursor_replays_nothing() {
        let mut stream = EventStream::new();
        stream.publish("message.new", message(1));
        assert_eq!(stream.replay(1), Replay::Frames(Vec::new()));
    }

    #[test]
    fn a_zero_cursor_replays_the_whole_ring() {
        let mut stream = EventStream::new();
        for n in 1..=3 {
            stream.publish("message.new", message(n));
        }
        let Replay::Frames(frames) = stream.replay(0) else {
            panic!("a fresh client is not a reset");
        };
        assert_eq!(frames.len(), 3);
    }

    /// §2.6: "if the cursor has aged out, the daemon sends `stream.reset`
    /// **first**, and the TUI invalidates and re-fetches rather than presenting
    /// a silently gapped timeline."
    #[test]
    fn an_aged_out_cursor_resets_rather_than_gapping() {
        let mut stream = EventStream::with_capacity(4);
        for n in 1..=10 {
            stream.publish("message.new", message(n));
        }
        // The ring holds 7..=10; a cursor at 3 has a real gap behind it.
        assert_eq!(stream.replay(3), Replay::Reset);
        // A cursor at 6 is exactly at the floor — the next frame it needs is 7,
        // which the ring still has, so this is *not* a gap.
        assert!(matches!(stream.replay(6), Replay::Frames(_)));
        assert_eq!(stream.replay(5), Replay::Reset, "one earlier is a gap");
    }

    /// A cursor **ahead** of the daemon is from a previous process whose
    /// sequence started over. Resetting is the honest answer: the two numbers
    /// do not refer to the same events, and replaying "nothing" would leave the
    /// client permanently stalled waiting for a `seq` that will be reused for a
    /// different frame.
    #[test]
    fn a_cursor_from_a_previous_daemon_process_resets() {
        let mut stream = EventStream::new();
        stream.publish("message.new", message(1));
        assert_eq!(stream.replay(9_999), Replay::Reset);
    }

    /// A restarted daemon with an empty ring must reset a client that had a
    /// cursor, rather than reporting it as caught up.
    #[test]
    fn an_empty_ring_resets_a_client_that_had_progress() {
        let stream = EventStream::new();
        assert_eq!(stream.replay(5), Replay::Reset);
        assert_eq!(
            stream.replay(0),
            Replay::Frames(Vec::new()),
            "a brand-new client is not a reset"
        );
    }

    #[test]
    fn the_ring_is_bounded() {
        let mut stream = EventStream::with_capacity(3);
        for n in 1..=10 {
            stream.publish("message.new", message(n));
        }
        assert_eq!(stream.len(), 3);
        assert_eq!(stream.latest_seq(), 10);
    }

    // ── [D-5] coalescing ──────────────────────────────────────────────────

    /// Latest-wins **per key**. Coalescing every `presence.update` onto one slot
    /// would mean one agent's beat erasing another's.
    #[test]
    fn coalescing_is_per_key_not_per_topic() {
        let mut stream = EventStream::new();
        let first = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "present"}),
        );
        let second = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-b", "state": "present"}),
        );
        assert_ne!(first, second, "two agents are two keys");
        assert_eq!(stream.len(), 2);
    }

    /// A superseded value replaces its predecessor **in place**, so the ring
    /// stays monotonic and a slow client sees the current value rather than
    /// every intermediate one.
    #[test]
    fn a_coalesced_topic_replaces_its_previous_value_in_place() {
        let mut stream = EventStream::new();
        let first = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "present"}),
        );
        let second = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "away"}),
        );
        assert_eq!(first, second, "the same key keeps its sequence position");
        assert_eq!(stream.len(), 1);

        let Replay::Frames(frames) = stream.replay(0) else {
            panic!()
        };
        assert_eq!(frames[0].payload["state"], serde_json::json!("away"));
    }

    /// Durable topics are never coalesced — two messages are two messages, even
    /// from the same author in the same channel.
    #[test]
    fn durable_topics_are_never_collapsed() {
        let mut stream = EventStream::new();
        stream.publish("message.new", serde_json::json!({"pubkey": "agent-a"}));
        stream.publish("message.new", serde_json::json!({"pubkey": "agent-a"}));
        assert_eq!(stream.len(), 2);
    }

    /// A coalescing-topic payload with no identifiable subject is treated as
    /// durable, matching [`policy_for`]'s rule that an unclassified frame must
    /// not silently become droppable.
    #[test]
    fn an_unkeyable_payload_is_not_collapsed() {
        let mut stream = EventStream::new();
        stream.publish("presence.update", serde_json::json!({"no": "pubkey"}));
        stream.publish("presence.update", serde_json::json!({"no": "pubkey"}));
        assert_eq!(stream.len(), 2);
    }

    /// Typing keys on (channel, pubkey): the same person typing in two channels
    /// is two facts.
    #[test]
    fn typing_keys_on_channel_and_pubkey_together() {
        let mut stream = EventStream::new();
        let a = stream.publish(
            "typing.start",
            serde_json::json!({"channel_id": "chan-1", "pubkey": "matt"}),
        );
        let b = stream.publish(
            "typing.start",
            serde_json::json!({"channel_id": "chan-2", "pubkey": "matt"}),
        );
        let repeat = stream.publish(
            "typing.start",
            serde_json::json!({"channel_id": "chan-1", "pubkey": "matt"}),
        );
        assert_ne!(a, b);
        assert_eq!(a, repeat);
    }

    /// Once a coalesced frame ages out of the ring, the next value for that key
    /// is genuinely new to any client still reading and must get a fresh
    /// sequence number — overwriting an evicted `seq` would be invisible.
    #[test]
    fn a_coalesced_key_gets_a_fresh_sequence_after_its_frame_ages_out() {
        let mut stream = EventStream::with_capacity(2);
        let first = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "present"}),
        );
        for n in 1..=3 {
            stream.publish("message.new", message(n));
        }
        let second = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "away"}),
        );
        assert!(
            second > first,
            "a value published after its predecessor aged out is new"
        );
    }

    // ── Content negotiation ───────────────────────────────────────────────

    /// ndjson is the default: simpler to parse, and no reconnect semantics
    /// baked into the transport that `?since=` would then have to fight.
    #[test]
    fn ndjson_is_the_default_and_sse_is_opt_in() {
        assert_eq!(Encoding::negotiate(None), Encoding::Ndjson);
        assert_eq!(Encoding::negotiate(Some("*/*")), Encoding::Ndjson);
        assert_eq!(
            Encoding::negotiate(Some("application/x-ndjson")),
            Encoding::Ndjson
        );
        assert_eq!(
            Encoding::negotiate(Some("text/event-stream")),
            Encoding::Sse
        );
        assert_eq!(
            Encoding::negotiate(Some("text/event-stream, */*;q=0.1")),
            Encoding::Sse
        );
    }

    #[test]
    fn ndjson_frames_are_one_line_each() {
        let frame = StreamFrame {
            seq: 7,
            topic: "message.new".into(),
            payload: serde_json::json!({"id": "abc"}),
        };
        let encoded = Encoding::Ndjson.encode(&frame);
        assert!(encoded.ends_with('\n'));
        assert_eq!(encoded.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(encoded.trim()).unwrap();
        assert_eq!(parsed["seq"], serde_json::json!(7));
        assert_eq!(parsed["type"], serde_json::json!("message.new"));
    }

    /// The SSE `id:` carries the same `seq` the body does, so an SSE client's
    /// own `Last-Event-ID` reconnect and the daemon's `?since=` cursor are the
    /// same number rather than two schemes to reconcile.
    #[test]
    fn sse_frames_carry_the_sequence_as_their_event_id() {
        let frame = StreamFrame {
            seq: 7,
            topic: "message.new".into(),
            payload: serde_json::json!({}),
        };
        let encoded = Encoding::Sse.encode(&frame);
        assert!(encoded.starts_with("id: 7\n"), "{encoded}");
        assert!(encoded.contains("data: {"), "{encoded}");
        assert!(
            encoded.ends_with("\n\n"),
            "SSE frames end with a blank line"
        );
    }

    #[test]
    fn content_types_match_the_negotiated_encoding() {
        assert_eq!(Encoding::Ndjson.content_type(), "application/x-ndjson");
        assert_eq!(Encoding::Sse.content_type(), "text/event-stream");
    }
}
