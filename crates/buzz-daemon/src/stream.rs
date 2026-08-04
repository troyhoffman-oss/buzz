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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The event-stream fan-out.
///
/// TODO(wave1, §4.1.1 deliverable 13): implement the ring at
/// [`EVENT_RING_CAPACITY`], per-client park queues honouring [`policy_for`],
/// `?since=` replay with [`StreamControl::Reset`] on an aged-out cursor, and
/// content negotiation between ndjson (default) and SSE.
#[derive(Debug, Default)]
pub struct EventStream {
    next_seq: u64,
}

impl EventStream {
    /// A stream whose first frame will carry `seq = 1`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next daemon-global sequence number.
    pub fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }
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
}
