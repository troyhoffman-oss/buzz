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
    /// Highest sequence number lost to **capacity eviction**.
    ///
    /// Tracked explicitly rather than read off the ring's oldest frame, because
    /// the two diverge: a coalescing supersede *withdraws* a frame without
    /// losing anything, which moves the ring's front without moving the floor.
    /// Deriving the floor from the front would then report a phantom gap and
    /// reset a client that had missed nothing — turning an ordinary presence
    /// beat into a full timeline invalidation.
    floor: u64,
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
            floor: 0,
        }
    }

    /// Allocate the next daemon-global sequence number.
    pub fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Publish a frame, assigning it a sequence number.
    ///
    /// Coalescing topics ([D-5]) **supersede** their previous value: the older
    /// frame is withdrawn from the ring and the new one takes a fresh sequence
    /// number at the end.
    ///
    /// # Why superseding, and not replacing in place
    ///
    /// Replacing in place — keeping the old `seq` and swapping the payload —
    /// looks like the obvious way to keep the ring monotonic, and it is wrong in
    /// a way that is invisible until it matters. A client that has already read
    /// past that `seq` is *caught up*, so the replay returns nothing and the new
    /// value never reaches it. Concretely: an agent goes offline, its
    /// `presence.update` overwrites a frame the operator's TUI already consumed,
    /// and the dot stays green forever. That is precisely the
    /// looks-alive-while-it-is-dead failure §1.3 property 3 forbids, reached by
    /// way of an optimization.
    ///
    /// Superseding gets both halves of what [D-5] actually asks for:
    ///
    /// - **Every client learns the latest value**, because it is newer than any
    ///   cursor.
    /// - **No client sees the intermediate ones**, because the superseded frame
    ///   is gone from the ring before the slow client ever reads it. That is
    ///   what "latest-wins per key" means, and dropping the intermediates is
    ///   semantically free precisely because the next value supersedes them.
    ///
    /// Withdrawing a frame leaves a hole in the ring's sequence numbers. That is
    /// fine and deliberate: [`Self::replay`] filters on `seq > since` rather
    /// than counting, and the aged-out detection reads [`Self::floor`], which
    /// only capacity eviction moves. A hole is not a gap — nothing was lost.
    pub fn publish(&mut self, topic: impl Into<String>, payload: serde_json::Value) -> u64 {
        let topic = topic.into();
        if policy_for(&topic) == DropPolicy::Coalescing {
            if let Some(key) = coalescing_key(&topic, &payload) {
                if let Some(superseded) = self.coalesced.remove(&key) {
                    self.ring.retain(|frame| frame.seq != superseded.seq);
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
                // Capacity eviction is real loss, so it moves the floor: a
                // client whose cursor is below it has a genuine gap and must
                // reset. Withdrawal by supersede does **not** move the floor,
                // because nothing was lost.
                self.floor = self.floor.max(evicted.seq);
                // Keep the coalescing index from pointing at a frame the ring
                // no longer holds; a stale entry there would make the next
                // publish try to withdraw a sequence that has already aged out.
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
        // `since` below the floor means frames were evicted that the client
        // never saw — a real gap, and `stream.reset` is the honest answer.
        // Compared against the **eviction** floor rather than the ring's oldest
        // frame, so a coalescing supersede (which loses nothing) never presents
        // as a gap.
        if since < self.floor {
            return Replay::Reset;
        }
        Replay::Frames(
            self.ring
                .iter()
                .filter(|frame| frame.seq > since)
                .cloned()
                .collect(),
        )
    }

    /// The highest sequence number lost to capacity eviction.
    ///
    /// A cursor at or above this has missed nothing; below it, the client must
    /// invalidate and re-fetch (§2.6).
    pub fn floor(&self) -> u64 {
        self.floor
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

    /// A superseded value **withdraws** its predecessor and takes a fresh
    /// sequence at the end: one frame in the ring, carrying the latest value.
    #[test]
    fn a_coalesced_topic_supersedes_its_previous_value() {
        let mut stream = EventStream::new();
        let first = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "present"}),
        );
        let second = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "away"}),
        );
        assert!(second > first, "the newer value gets a newer sequence");
        assert_eq!(stream.len(), 1, "and the older one is gone from the ring");

        let Replay::Frames(frames) = stream.replay(0) else {
            panic!()
        };
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload["state"], serde_json::json!("away"));
    }

    /// **The bug this design exists to prevent.**
    ///
    /// Replacing in place — keeping the old `seq` and swapping the payload —
    /// is the obvious optimization and it silently strands every caught-up
    /// client: the replay returns nothing because the client is already past
    /// that sequence. An agent goes offline and the operator's dot stays green
    /// forever, which is the looks-alive-while-it-is-dead failure §1.3 property
    /// 3 forbids, reached by way of an optimization.
    #[test]
    fn a_superseding_value_reaches_a_client_that_was_already_caught_up() {
        let mut stream = EventStream::new();
        let seq = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "present"}),
        );
        // The client reads to here and is caught up.
        assert_eq!(stream.replay(seq), Replay::Frames(Vec::new()));

        stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "offline"}),
        );

        let Replay::Frames(frames) = stream.replay(seq) else {
            panic!("nothing was lost, so this must not be a reset");
        };
        assert_eq!(
            frames.len(),
            1,
            "the caught-up client must learn the agent went offline"
        );
        assert_eq!(frames[0].payload["state"], serde_json::json!("offline"));
    }

    /// Withdrawal leaves a hole in the sequence, and a hole is **not** a gap:
    /// nothing was lost, so a client spanning it must not be reset.
    #[test]
    fn a_withdrawn_sequence_is_a_hole_not_a_gap() {
        let mut stream = EventStream::new();
        let coalesced = stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "present"}),
        );
        stream.publish("message.new", message(1));
        // Supersede the first frame, leaving `coalesced` withdrawn.
        stream.publish(
            "presence.update",
            serde_json::json!({"pubkey": "agent-a", "state": "away"}),
        );

        assert_eq!(stream.floor(), 0, "no capacity eviction happened");
        let Replay::Frames(frames) = stream.replay(coalesced - 1) else {
            panic!("a hole must not present as an aged-out cursor");
        };
        assert_eq!(
            frames.len(),
            2,
            "the message and the current presence value"
        );
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
    /// is two facts, and both survive. A repeat in one channel supersedes only
    /// that channel's frame.
    #[test]
    fn typing_keys_on_channel_and_pubkey_together() {
        let mut stream = EventStream::new();
        stream.publish(
            "typing.start",
            serde_json::json!({"channel_id": "chan-1", "pubkey": "matt"}),
        );
        stream.publish(
            "typing.start",
            serde_json::json!({"channel_id": "chan-2", "pubkey": "matt"}),
        );
        assert_eq!(stream.len(), 2, "two channels are two keys");

        stream.publish(
            "typing.start",
            serde_json::json!({"channel_id": "chan-1", "pubkey": "matt"}),
        );
        assert_eq!(
            stream.len(),
            2,
            "the repeat superseded chan-1's frame and left chan-2's alone"
        );
        let Replay::Frames(frames) = stream.replay(0) else {
            panic!()
        };
        let channels: std::collections::BTreeSet<&str> = frames
            .iter()
            .filter_map(|f| f.payload["channel_id"].as_str())
            .collect();
        assert_eq!(channels, ["chan-1", "chan-2"].into_iter().collect());
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
