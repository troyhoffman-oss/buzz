//! Channel discovery, the roster cache, and the ambient state the L1 layer
//! filters on.
//!
//! Implements Wave-1 daemon deliverable 3 (`DESIGN.md` §4.1.1): 39002
//! `#p=self` → `#d` uuids → 39000 batch, kept live from 44100/44101.
//!
//! Discovery mirrors `HarnessRelay::discover_channels`
//! (`crates/buzz-acp/src/relay.rs:669`) and its `merge_discovered_channels`
//! (`relay.rs:171`) — including the archived-channel drop, which happens at
//! *merge* time rather than at query time because `archived` is a tag on the
//! 39000 metadata and the 39002 membership event that discovered the uuid does
//! not carry it.
//!
//! §1.4: **multi-community aggregation stays out of the daemon.** One daemon
//! per (relay, identity); the TUI opens N and merges. Because each daemon is a
//! separate process with a separate cache and a separate relay budget, the
//! entire class of bug that the desktop's `resetCommunityState()` exists to
//! prevent — module-level caches leaking across a relay boundary — cannot occur
//! here. There is no shared memory to leak through (§2.2).
//!
//! # What the navigation layer asks of this module
//!
//! `nav/NAVIGATION.md` §1 puts the channel list at L1 and §2.1 puts two ambient
//! signals in the statusline: unread counts and *where agents are working*. The
//! second one is called out as "IA §5.3's highest-value ambient signal, given no
//! keyboard reachability at all by the desktop" — so [`Channel::agents_working`]
//! is a first-class field on the list read, not something the client derives by
//! cross-referencing the fleet. Deriving it client-side would mean the channel
//! list and the drawer could disagree about which channels are live.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::identity::Identity;
use crate::rest::RestClient;

/// Channel membership kind, queried with `#p = self` to discover channels.
pub const KIND_CHANNEL_MEMBERSHIP: u32 = buzz_core::kind::KIND_NIP29_GROUP_MEMBERS;

/// Channel metadata kind.
///
/// §"Common Gotchas" 1: kind **39000** for channel metadata, not 41 — kind 41 is
/// NIP-01 and unused here.
pub const KIND_CHANNEL_METADATA: u32 = buzz_core::kind::KIND_NIP29_GROUP_METADATA;

/// Member-added notification, keeping the roster live.
pub const KIND_MEMBER_ADDED: u32 = buzz_core::kind::KIND_MEMBER_ADDED_NOTIFICATION;

/// Member-removed notification, keeping the roster live.
pub const KIND_MEMBER_REMOVED: u32 = buzz_core::kind::KIND_MEMBER_REMOVED_NOTIFICATION;

/// A cached channel, as served by `GET /channel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// Channel uuid, as carried in the `#h` tag (NIP-29 group tag) rather than
    /// an `#e` tag — filters and queries scope to `h`.
    pub id: String,
    /// Display name without the leading `#`.
    pub name: String,
    /// Channel topic line.
    pub topic: Option<String>,
    /// Declared channel type (`channel`, `dm`, `forum`, …), or `unknown` when
    /// the metadata event has not arrived.
    ///
    /// `unknown` is deliberately not collapsed into `channel`: §2.4's presence
    /// argument applies here too — "we have not heard yet" and "it is a regular
    /// channel" are different facts, and the second one is a guess.
    pub channel_type: String,
    /// Member count as of the last roster read.
    pub member_count: u32,
    /// Conversational unread count (§4.1.1 deliverable 5).
    ///
    /// Gated by [`crate::timeline::is_conversational_unread_kind`], so system,
    /// job, and huddle rows never create phantom unreads.
    pub unread: u32,
    /// Unread messages that mention this identity.
    pub mentions: u32,
    /// Names of agents currently working in this channel.
    ///
    /// The ambient signal `NAVIGATION.md` §2.1 row 3 renders and `↓` opens.
    /// Served from the daemon so the channel list and the drawer cannot
    /// disagree.
    pub agents_working: Vec<String>,
    /// Whether the metadata marked this channel archived. Archived channels are
    /// dropped from discovery; the field exists so a direct `GET /channel/{id}`
    /// can still answer honestly.
    pub archived: bool,
}

impl Channel {
    /// A channel known only by uuid, before its 39000 metadata arrives.
    pub fn unknown(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: "unknown".into(),
            topic: None,
            channel_type: "unknown".into(),
            member_count: 0,
            unread: 0,
            mentions: 0,
            agents_working: Vec::new(),
            archived: false,
        }
    }

    /// Whether this channel carries anything the attention ladder should
    /// surface (`NAVIGATION.md` §1.1: default selection lands on the first
    /// attention-bearing row).
    pub fn has_attention(&self) -> bool {
        self.unread > 0 || self.mentions > 0 || !self.agents_working.is_empty()
    }
}

/// Build the discovery filter: 39002 scoped to `#p = self`.
///
/// `kinds` is explicit, per §2.4's global invariant. The `#p` scope is not an
/// optimization — an unscoped read would return every membership event on the
/// relay, which is a different query with a much larger answer.
pub fn build_discovery_filter(self_pubkey: &str) -> serde_json::Value {
    serde_json::json!({
        "kinds": [KIND_CHANNEL_MEMBERSHIP],
        "#p": [self_pubkey],
    })
}

/// Build the metadata batch filter for the discovered uuids.
pub fn build_metadata_filter(channel_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "kinds": [KIND_CHANNEL_METADATA],
        "#d": channel_ids,
    })
}

/// Build the live membership filter (44100/44101).
///
/// Both kinds are `P_GATED_KIND`s, so the `#p = self` scope is **mandatory**,
/// not a narrowing: without it the relay closes the subscription as
/// `restricted:` and the roster silently stops updating.
pub fn build_membership_filter(self_pubkey: &str, since: Option<u64>) -> serde_json::Value {
    let mut filter = serde_json::json!({
        "kinds": [KIND_MEMBER_ADDED, KIND_MEMBER_REMOVED],
        "#p": [self_pubkey],
    });
    if let Some(since) = since {
        filter["since"] = serde_json::json!(since);
    }
    filter
}

/// Read a tag's first value off an event.
fn tag_value<'a>(event: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    event
        .get("tags")?
        .as_array()?
        .iter()
        .find(|tag| tag.get(0).and_then(serde_json::Value::as_str) == Some(name))
        .and_then(|tag| tag.get(1))
        .and_then(serde_json::Value::as_str)
}

/// Every value of a repeated tag.
fn tag_values<'a>(event: &'a serde_json::Value, name: &str) -> Vec<&'a str> {
    event
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter(|tag| tag.get(0).and_then(serde_json::Value::as_str) == Some(name))
                .filter_map(|tag| tag.get(1).and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

/// Extract the channel uuid from a 39002 membership event's `#d` tag.
pub fn channel_id_from_membership(event: &serde_json::Value) -> Option<String> {
    let raw = tag_value(event, "d")?;
    // Parsed rather than passed through: a `d` tag that is not a uuid is not a
    // channel, and forwarding it would put a filter on the wire that matches
    // nothing while looking like it should.
    uuid::Uuid::parse_str(raw).ok().map(|u| u.to_string())
}

/// Merge 39002 uuids with their 39000 metadata, dropping archived channels.
///
/// Ported from `merge_discovered_channels` (`crates/buzz-acp/src/relay.rs:171`).
/// A uuid with no metadata event survives as [`Channel::unknown`] rather than
/// being dropped: the membership event is authoritative about *membership*, and
/// dropping the channel because its name has not arrived would make the list
/// flicker on every cold start.
pub fn merge_discovered(
    channel_ids: Vec<String>,
    metadata_events: &[serde_json::Value],
) -> Vec<Channel> {
    let mut meta: BTreeMap<String, Channel> = BTreeMap::new();
    let mut archived: BTreeSet<String> = BTreeSet::new();

    for event in metadata_events {
        let Some(id) = tag_value(event, "d") else {
            continue;
        };
        if tag_value(event, "archived") == Some("true") {
            archived.insert(id.to_string());
            continue;
        }
        let members = tag_values(event, "p");
        meta.insert(
            id.to_string(),
            Channel {
                id: id.to_string(),
                name: tag_value(event, "name").unwrap_or("unknown").to_string(),
                topic: tag_value(event, "topic").map(str::to_string),
                channel_type: channel_type_from_event(event),
                member_count: members.len() as u32,
                unread: 0,
                mentions: 0,
                agents_working: Vec::new(),
                archived: false,
            },
        );
    }

    channel_ids
        .into_iter()
        .filter(|id| !archived.contains(id))
        .map(|id| meta.remove(&id).unwrap_or_else(|| Channel::unknown(id)))
        .collect()
}

/// Derive the channel type from its 39000 tags.
///
/// A declared `t` tag wins, and absence is `unknown` rather than a default —
/// mirroring the harness's `channel_type_from_tags`.
fn channel_type_from_event(event: &serde_json::Value) -> String {
    tag_value(event, "t")
        .or_else(|| tag_value(event, "type"))
        .unwrap_or("unknown")
        .to_string()
}

/// The channel cache: discovery, live roster maintenance, and ambient state.
#[derive(Debug, Default)]
pub struct Channels {
    channels: BTreeMap<String, Channel>,
    /// Members per channel, the directory `/mention/candidates` resolves over.
    rosters: BTreeMap<String, BTreeSet<String>>,
    /// Agent name per pubkey, so an `agents_working` entry can be rendered
    /// without a second lookup.
    agent_names: BTreeMap<String, String>,
    /// Which channel each agent is currently working in, keyed by agent pubkey.
    working: BTreeMap<String, String>,
}

impl Channels {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run the two-round-trip discovery of §4.1.1 deliverable 3.
    ///
    /// Round 1 finds the uuids this identity is a member of; round 2 batches
    /// their metadata. Two round trips rather than one because there is no
    /// filter that expresses "the 39000 events whose `d` matches the `d` of the
    /// 39002 events that carry my `p`".
    pub async fn discover(&mut self, rest: &RestClient, identity: &Identity) -> Result<usize> {
        let memberships = rest
            .query(identity, &build_discovery_filter(&identity.pubkey))
            .await?;

        let ids: Vec<String> = memberships
            .iter()
            .filter_map(channel_id_from_membership)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        // Record the rosters while the membership events are in hand: the same
        // query that discovers a channel also carries its member list, and a
        // second fetch for it would be a round trip for data already read.
        for event in &memberships {
            if let Some(id) = channel_id_from_membership(event) {
                let members: BTreeSet<String> = tag_values(event, "p")
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                self.rosters.insert(id, members);
            }
        }

        if ids.is_empty() {
            self.channels.clear();
            return Ok(0);
        }

        let metadata = rest.query(identity, &build_metadata_filter(&ids)).await?;

        for channel in merge_discovered(ids, &metadata) {
            self.merge_preserving_ambient(channel);
        }
        Ok(self.channels.len())
    }

    /// Insert a rediscovered channel without losing its ambient state.
    ///
    /// Unread counts and working agents come from other sources (read-state and
    /// the observer pipeline). A rediscovery that zeroed them would blank the
    /// sidebar on every reconnect — the exact "looks idle while it is not"
    /// failure §1.3 property 3 forbids, one layer down.
    ///
    /// Public because [`crate::wire::hydrate_channels`] runs the cold-start walk
    /// against a **detached** cache — so the two relay round trips happen with
    /// no state lock held — and then merges the answer back here. That merge
    /// needs exactly this ambient-preserving semantic; [`Self::upsert`] would
    /// blank the unread counts of every channel on every rediscovery.
    pub fn merge_preserving_ambient(&mut self, mut channel: Channel) {
        if let Some(existing) = self.channels.get(&channel.id) {
            channel.unread = existing.unread;
            channel.mentions = existing.mentions;
            channel.agents_working = existing.agents_working.clone();
        }
        if channel.member_count == 0 {
            if let Some(roster) = self.rosters.get(&channel.id) {
                channel.member_count = roster.len() as u32;
            }
        }
        self.channels.insert(channel.id.clone(), channel);
    }

    /// Every cached channel, sorted attention-first then by name.
    ///
    /// `NAVIGATION.md` §1.1: entering a layer selects "the first
    /// attention-bearing row", which is only well-defined if the order puts
    /// those rows first and is stable.
    pub fn list(&self) -> Vec<Channel> {
        let mut out: Vec<Channel> = self.channels.values().cloned().collect();
        out.sort_by(|a, b| {
            b.mentions
                .cmp(&a.mentions)
                .then_with(|| b.unread.cmp(&a.unread))
                .then_with(|| b.agents_working.len().cmp(&a.agents_working.len()))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// One channel by uuid.
    pub fn get(&self, channel_id: &str) -> Option<&Channel> {
        self.channels.get(channel_id)
    }

    /// Insert or replace a channel record.
    pub fn upsert(&mut self, channel: Channel) {
        self.channels.insert(channel.id.clone(), channel);
    }

    /// The roster for a channel, as `/mention/candidates` reads it.
    pub fn roster(&self, channel_id: &str) -> Option<&BTreeSet<String>> {
        self.rosters.get(channel_id)
    }

    /// Every roster this cache holds, keyed by channel uuid.
    ///
    /// Exists for the same reason [`Self::merge_preserving_ambient`] is public:
    /// the cold-start walk in [`crate::wire::hydrate_channels`] discovers into a
    /// detached cache and has to move the rosters it found back into the live
    /// one. Rosters seed the presence subscription (`plan_subscriptions` builds
    /// its author list from exactly this), so a walk that carried the channels
    /// across but not their members would leave every peer `unknown`.
    pub fn rosters(&self) -> &BTreeMap<String, BTreeSet<String>> {
        &self.rosters
    }

    /// Replace a channel's roster wholesale.
    pub fn set_roster(&mut self, channel_id: impl Into<String>, members: BTreeSet<String>) {
        let id = channel_id.into();
        let count = members.len() as u32;
        self.rosters.insert(id.clone(), members);
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.member_count = count;
        }
    }

    /// Apply a live 44100 / 44101 membership notification.
    ///
    /// Returns `true` when the roster actually changed, so the caller only
    /// emits a `channel.member` stream frame for a real change — a re-delivered
    /// notification after a reconnect replay is common and must not look like
    /// new activity.
    pub fn apply_membership(&mut self, event: &serde_json::Value) -> bool {
        let Some(kind) = event.get("kind").and_then(serde_json::Value::as_u64) else {
            return false;
        };
        let Some(channel_id) = tag_value(event, "h")
            .or_else(|| tag_value(event, "d"))
            .map(str::to_string)
        else {
            return false;
        };
        let members = tag_values(event, "p");
        let Some(pubkey) = members.first().map(|s| s.to_string()) else {
            return false;
        };

        let roster = self.rosters.entry(channel_id.clone()).or_default();
        let changed = match kind as u32 {
            KIND_MEMBER_ADDED => roster.insert(pubkey),
            KIND_MEMBER_REMOVED => roster.remove(&pubkey),
            _ => false,
        };
        if changed {
            let count = roster.len() as u32;
            if let Some(channel) = self.channels.get_mut(&channel_id) {
                channel.member_count = count;
            }
        }
        changed
    }

    /// Register an agent's display name, so `agents_working` can carry names.
    pub fn register_agent(&mut self, pubkey: impl Into<String>, name: impl Into<String>) {
        self.agent_names.insert(pubkey.into(), name.into());
    }

    /// Record that `agent_pubkey` is working in `channel_id`, or nowhere.
    ///
    /// Returns the channels whose `agents_working` changed, so the caller emits
    /// exactly the ambient frames that moved. An agent moving between channels
    /// changes **two** rows, which is why this returns a list rather than a
    /// bool.
    pub fn set_agent_working(
        &mut self,
        agent_pubkey: &str,
        channel_id: Option<&str>,
    ) -> Vec<String> {
        let previous = self.working.get(agent_pubkey).cloned();
        if previous.as_deref() == channel_id {
            return Vec::new();
        }
        match channel_id {
            Some(id) => self
                .working
                .insert(agent_pubkey.to_string(), id.to_string()),
            None => self.working.remove(agent_pubkey),
        };
        let mut touched: Vec<String> = Vec::new();
        if let Some(prev) = previous {
            touched.push(prev);
        }
        if let Some(next) = channel_id {
            touched.push(next.to_string());
        }
        for id in &touched {
            self.recompute_working(id);
        }
        touched
    }

    fn recompute_working(&mut self, channel_id: &str) {
        let mut names: Vec<String> = self
            .working
            .iter()
            .filter(|(_, ch)| ch.as_str() == channel_id)
            .map(|(pk, _)| {
                self.agent_names
                    .get(pk)
                    .cloned()
                    .unwrap_or_else(|| pk.clone())
            })
            .collect();
        names.sort();
        if let Some(channel) = self.channels.get_mut(channel_id) {
            channel.agents_working = names;
        }
    }

    /// Set a channel's unread counts, as recomputed by [`crate::readstate`].
    pub fn set_unread(&mut self, channel_id: &str, unread: u32, mentions: u32) {
        if let Some(channel) = self.channels.get_mut(channel_id) {
            channel.unread = unread;
            channel.mentions = mentions;
        }
    }

    /// Total unread and mention counts across every channel — the two numbers
    /// `NAVIGATION.md` §2.1 row 2 renders.
    pub fn totals(&self) -> (u32, u32) {
        self.channels.values().fold((0, 0), |(u, m), channel| {
            (u + channel.unread, m + channel.mentions)
        })
    }

    /// How many channels are cached.
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Whether discovery has found nothing.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn membership(channel_id: &str, members: &[&str]) -> serde_json::Value {
        let mut tags = vec![serde_json::json!(["d", channel_id])];
        tags.extend(members.iter().map(|m| serde_json::json!(["p", m])));
        serde_json::json!({"kind": KIND_CHANNEL_MEMBERSHIP, "tags": tags})
    }

    fn metadata(channel_id: &str, name: &str, extra: &[serde_json::Value]) -> serde_json::Value {
        let mut tags = vec![
            serde_json::json!(["d", channel_id]),
            serde_json::json!(["name", name]),
        ];
        tags.extend(extra.iter().cloned());
        serde_json::json!({"kind": KIND_CHANNEL_METADATA, "tags": tags})
    }

    fn uuid(n: u8) -> String {
        uuid::Uuid::from_bytes([n; 16]).to_string()
    }

    /// "Common Gotchas" 1: 39000, never 41.
    #[test]
    fn channel_metadata_is_39000_not_41() {
        assert_eq!(KIND_CHANNEL_METADATA, 39_000);
        assert_ne!(KIND_CHANNEL_METADATA, 41);
    }

    #[test]
    fn discovery_and_roster_kinds_match_the_design() {
        assert_eq!(KIND_CHANNEL_MEMBERSHIP, 39_002);
        assert_eq!(KIND_MEMBER_ADDED, 44_100);
        assert_eq!(KIND_MEMBER_REMOVED, 44_101);
    }

    /// §2.4's global invariant, on every filter this module builds.
    #[test]
    fn every_filter_carries_explicit_kinds() {
        for filter in [
            build_discovery_filter(&"aa".repeat(32)),
            build_metadata_filter(&[uuid(1)]),
            build_membership_filter(&"aa".repeat(32), None),
        ] {
            crate::search::assert_explicit_kinds(&filter, "channels").unwrap();
        }
    }

    /// 44100/44101 are `P_GATED_KIND`s: without `#p = self` the relay closes
    /// the subscription and the roster silently stops updating.
    #[test]
    fn the_membership_filter_is_scoped_to_self() {
        let me = "aa".repeat(32);
        let filter = build_membership_filter(&me, Some(1_700_000_000));
        assert_eq!(filter["#p"], serde_json::json!([me]));
        assert_eq!(filter["since"], serde_json::json!(1_700_000_000));
    }

    /// A `d` tag that is not a uuid is not a channel. Passing it through would
    /// put a filter on the wire that matches nothing while looking correct.
    #[test]
    fn a_non_uuid_d_tag_is_not_a_channel() {
        assert!(channel_id_from_membership(&membership("not-a-uuid", &[])).is_none());
        assert_eq!(
            channel_id_from_membership(&membership(&uuid(7), &[])),
            Some(uuid(7))
        );
    }

    /// Ported from `merge_discovered_channels`: archived channels are dropped
    /// at merge time, because the 39002 that discovered the uuid cannot know.
    #[test]
    fn archived_channels_are_dropped_at_merge() {
        let live = uuid(1);
        let gone = uuid(2);
        let merged = merge_discovered(
            vec![live.clone(), gone.clone()],
            &[
                metadata(&live, "engineering", &[]),
                metadata(&gone, "old", &[serde_json::json!(["archived", "true"])]),
            ],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, live);
    }

    /// A uuid with no metadata survives as `unknown` rather than vanishing —
    /// membership is authoritative about membership.
    #[test]
    fn a_channel_without_metadata_survives_as_unknown() {
        let id = uuid(3);
        let merged = merge_discovered(vec![id.clone()], &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "unknown");
        assert_eq!(
            merged[0].channel_type, "unknown",
            "an unknown type must not be guessed as `channel`"
        );
    }

    #[test]
    fn metadata_supplies_name_topic_type_and_member_count() {
        let id = uuid(4);
        let merged = merge_discovered(
            vec![id.clone()],
            &[metadata(
                &id,
                "engineering",
                &[
                    serde_json::json!(["topic", "relay + desktop"]),
                    serde_json::json!(["t", "channel"]),
                    serde_json::json!(["p", "aa".repeat(32)]),
                    serde_json::json!(["p", "bb".repeat(32)]),
                ],
            )],
        );
        let channel = &merged[0];
        assert_eq!(channel.name, "engineering");
        assert_eq!(channel.topic.as_deref(), Some("relay + desktop"));
        assert_eq!(channel.channel_type, "channel");
        assert_eq!(channel.member_count, 2);
    }

    /// A live 44100 adds to the roster and moves the member count; a replayed
    /// duplicate reports **no change**, so the caller does not emit a frame
    /// that looks like new activity.
    #[test]
    fn membership_notifications_are_idempotent() {
        let id = uuid(5);
        let mut channels = Channels::new();
        channels.upsert(Channel::unknown(&id));

        let added = serde_json::json!({
            "kind": KIND_MEMBER_ADDED,
            "tags": [["h", id], ["p", "cc".repeat(32)]],
        });
        assert!(channels.apply_membership(&added));
        assert!(
            !channels.apply_membership(&added),
            "a replayed notification must not report a change"
        );
        assert_eq!(channels.get(&id).unwrap().member_count, 1);

        let removed = serde_json::json!({
            "kind": KIND_MEMBER_REMOVED,
            "tags": [["h", id], ["p", "cc".repeat(32)]],
        });
        assert!(channels.apply_membership(&removed));
        assert!(!channels.apply_membership(&removed));
        assert_eq!(channels.get(&id).unwrap().member_count, 0);
    }

    /// `NAVIGATION.md` §2.1: agents-working is served by the daemon so the
    /// channel list and the drawer cannot disagree. An agent *moving* changes
    /// two rows.
    #[test]
    fn moving_an_agent_updates_both_channels() {
        let from = uuid(6);
        let to = uuid(7);
        let mut channels = Channels::new();
        channels.upsert(Channel::unknown(&from));
        channels.upsert(Channel::unknown(&to));
        channels.register_agent("pk-claude", "claude-1");

        assert_eq!(
            channels.set_agent_working("pk-claude", Some(&from)),
            vec![from.clone()]
        );
        assert_eq!(channels.get(&from).unwrap().agents_working, ["claude-1"]);

        let touched = channels.set_agent_working("pk-claude", Some(&to));
        assert_eq!(touched, vec![from.clone(), to.clone()]);
        assert!(channels.get(&from).unwrap().agents_working.is_empty());
        assert_eq!(channels.get(&to).unwrap().agents_working, ["claude-1"]);

        assert_eq!(
            channels.set_agent_working("pk-claude", None),
            vec![to.clone()]
        );
        assert!(channels.get(&to).unwrap().agents_working.is_empty());
    }

    /// Setting the same channel twice is not a change — no frame, no flicker.
    #[test]
    fn re_asserting_the_same_working_channel_changes_nothing() {
        let id = uuid(8);
        let mut channels = Channels::new();
        channels.upsert(Channel::unknown(&id));
        channels.set_agent_working("pk", Some(&id));
        assert!(channels.set_agent_working("pk", Some(&id)).is_empty());
    }

    /// An agent with no registered name still renders — as its pubkey. Dropping
    /// it would make "3 agents working" disagree with a list of two names.
    #[test]
    fn an_unnamed_agent_still_appears_in_the_ambient_state() {
        let id = uuid(9);
        let mut channels = Channels::new();
        channels.upsert(Channel::unknown(&id));
        channels.set_agent_working("pk-unnamed", Some(&id));
        assert_eq!(channels.get(&id).unwrap().agents_working, ["pk-unnamed"]);
    }

    /// A rediscovery must not blank the ambient state — that would clear the
    /// sidebar on every reconnect.
    #[test]
    fn rediscovery_preserves_unread_and_working_state() {
        let id = uuid(10);
        let mut channels = Channels::new();
        channels.upsert(Channel::unknown(&id));
        channels.set_unread(&id, 5, 2);
        channels.register_agent("pk", "claude-1");
        channels.set_agent_working("pk", Some(&id));

        let mut rediscovered = Channel::unknown(&id);
        rediscovered.name = "engineering".into();
        channels.merge_preserving_ambient(rediscovered);

        let channel = channels.get(&id).unwrap();
        assert_eq!(channel.name, "engineering", "metadata still updates");
        assert_eq!(channel.unread, 5);
        assert_eq!(channel.mentions, 2);
        assert_eq!(channel.agents_working, ["claude-1"]);
    }

    /// `NAVIGATION.md` §1.1: the first attention-bearing row is only
    /// well-defined if the order is attention-first and stable.
    #[test]
    fn the_list_is_ordered_attention_first_and_is_stable() {
        let mut channels = Channels::new();
        for (n, name) in [(1u8, "zulu"), (2, "alpha"), (3, "bravo"), (4, "charlie")] {
            let mut channel = Channel::unknown(uuid(n));
            channel.name = name.into();
            channels.upsert(channel);
        }
        channels.set_unread(&uuid(1), 3, 0);
        channels.set_unread(&uuid(3), 1, 2);
        channels.register_agent("pk", "claude-1");
        channels.set_agent_working("pk", Some(&uuid(4)));

        let order: Vec<String> = channels.list().into_iter().map(|c| c.name).collect();
        assert_eq!(
            order,
            ["bravo", "zulu", "charlie", "alpha"],
            "mentions, then unread, then working, then name"
        );
        let again: Vec<String> = channels.list().into_iter().map(|c| c.name).collect();
        assert_eq!(order, again, "the order must be stable across calls");
    }

    #[test]
    fn attention_covers_all_three_signals() {
        let mut channel = Channel::unknown(uuid(1));
        assert!(!channel.has_attention());
        channel.unread = 1;
        assert!(channel.has_attention());
        channel.unread = 0;
        channel.mentions = 1;
        assert!(channel.has_attention());
        channel.mentions = 0;
        channel.agents_working = vec!["claude-1".into()];
        assert!(channel.has_attention());
    }

    /// `NAVIGATION.md` §2.1 row 2 renders these two numbers.
    #[test]
    fn totals_sum_across_channels() {
        let mut channels = Channels::new();
        channels.upsert(Channel::unknown(uuid(1)));
        channels.upsert(Channel::unknown(uuid(2)));
        channels.set_unread(&uuid(1), 8, 1);
        channels.set_unread(&uuid(2), 4, 2);
        assert_eq!(channels.totals(), (12, 3));
    }
}
