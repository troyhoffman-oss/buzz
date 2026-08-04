//! Unread and read-state (NIP-RS).
//!
//! Implements Wave-1 daemon deliverable 5 (`DESIGN.md` §4.1.1): kind 30078
//! `d=read-state:<slot>`, NIP-44 self-encrypted `{v:1, client_id, contexts}`,
//! with context keys as bare `<channelUuid>` / `thread:<id>` / `msg:<id>`.
//!
//! §5.2 calls this "the highest-risk port", and §4.1.4 exit criterion 6 gates
//! the wave on the unread divider surviving a reconnect burst, a tier change, a
//! daemon restart, and a second client marking a different channel read.

use serde::{Deserialize, Serialize};

/// Maximum encrypted bytes per read-state slot (§4.1.1 deliverable 5).
pub const MAX_SLOT_BYTES: usize = 32 * 1024;

/// Maximum number of slots before the oldest contexts are pruned.
pub const MAX_SLOTS: usize = 8;

/// Maximum tracked contexts across all slots.
pub const MAX_CONTEXTS: usize = 10_000;

/// Low-water mark the context cap trims down to.
///
/// The gap between this and [`MAX_CONTEXTS`] is what makes eviction amortized:
/// trimming to exactly the cap makes every subsequent `mark` one-over and pays
/// a full scan-and-sort per read forever. See
/// [`ReadState::enforce_context_cap`].
pub const CONTEXT_LOW_WATER: usize = 9_000;

/// How many new contexts must arrive before retrying a cap scan that could not
/// make progress. See [`ReadState::enforce_context_cap`].
pub const CAP_RECHECK_STRIDE: usize = 1_000;

/// Horizon after which `msg:` and `thread:` contexts are pruned, in days.
pub const CONTEXT_HORIZON_DAYS: u32 = 7;

/// Debounce before publishing a read-state update, in milliseconds.
pub const PUBLISH_DEBOUNCE_MS: u64 = 5_000;

/// A read-state context key (§4.1.1 deliverable 5).
///
/// Keys are **bare**: a channel context is the raw uuid, not `channel:<uuid>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextKey {
    /// Any of the three forms, held as its wire string.
    Raw(String),
}

/// `d`-tag prefix of a read-state slot event.
///
/// Origin: `desktop/src/features/channels/readState/readStateFormat.ts:6`.
pub const READ_STATE_D_TAG_PREFIX: &str = "read-state:";

/// Context-key prefix for a per-**message** marker.
///
/// One grow-only marker per reply id, so reading an ancestor never covers a
/// descendant. Distinct from [`THREAD_PREFIX`] so the parent resolver and the
/// horizon prune can tell the two families apart.
pub const MSG_PREFIX: &str = "msg:";

/// Context-key prefix for a per-**thread** marker.
pub const THREAD_PREFIX: &str = "thread:";

/// Maximum bytes of one context key.
pub const MAX_CONTEXT_KEY_BYTES: usize = 256;

/// Largest accepted marker value — a `u32` of unix seconds, matching
/// `sanitizeContexts`.
pub const MAX_MARKER: u64 = 4_294_967_295;

/// The NIP-44 self-encrypted blob of one slot.
///
/// `{v: 1, client_id, contexts}` verbatim from `ReadStateBlob`
/// (`readStateFormat.ts:1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadStateBlob {
    /// Schema version; must be 1.
    pub v: u32,
    /// Publisher identity, so a second client squatting this slot's `d` tag is
    /// detectable rather than a silent clobber (§2.3).
    pub client_id: String,
    /// Context key → unix-seconds marker.
    pub contexts: std::collections::BTreeMap<String, u64>,
}

impl ReadStateBlob {
    /// Whether the blob is structurally valid, per `isValidBlob`.
    ///
    /// A blob that fails this is **ignored**, not partially applied: a
    /// half-parsed read-state moves some markers and not others, which is worse
    /// than moving none.
    pub fn is_valid(&self) -> bool {
        self.v == 1
            && !self.client_id.is_empty()
            && self.client_id.len() <= 64
            && self.contexts.len() <= MAX_CONTEXTS
    }
}

/// Drop context entries that could not have been written by a conforming
/// client, per `sanitizeContexts` (`readStateFormat.ts:96`).
///
/// Filtering rather than rejecting: one malformed entry from some other client
/// must not discard an otherwise good blob, because that blob carries this
/// device's own markers too.
pub fn sanitize_contexts(
    contexts: std::collections::BTreeMap<String, u64>,
) -> std::collections::BTreeMap<String, u64> {
    contexts
        .into_iter()
        .filter(|(key, value)| key.len() <= MAX_CONTEXT_KEY_BYTES && *value <= MAX_MARKER)
        .collect()
}

/// The `d` tag for a slot id.
pub fn slot_d_tag(slot_id: &str) -> String {
    format!("{READ_STATE_D_TAG_PREFIX}{slot_id}")
}

/// Whether a `d` tag names a read-state slot, per `isValidReadStateDTag`.
pub fn is_read_state_d_tag(value: &str) -> bool {
    let Some(slot) = value.strip_prefix(READ_STATE_D_TAG_PREFIX) else {
        return false;
    };
    !slot.is_empty() && slot.len() <= 64 && slot.is_ascii()
}

/// Whether a context key names a thread.
pub fn is_thread_context(key: &str) -> bool {
    is_event_scoped(key, THREAD_PREFIX)
}

/// Whether a context key names a message.
pub fn is_msg_context(key: &str) -> bool {
    is_event_scoped(key, MSG_PREFIX)
}

fn is_event_scoped(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix).is_some_and(|id| {
        id.len() == 64
            && id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    })
}

/// Resolves a context key to its parent, from the **event graph**.
///
/// §4.1.1 deliverable 5: the thread→channel parent link is "derived from the
/// event graph at evaluation time, **never serialized**". Serializing it would
/// make the link a second source of truth that goes stale the moment a thread
/// moves or a root is deleted — and a stale parent link silently mis-marks a
/// whole channel read.
pub type ParentResolver<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The read-state frontier (§4.1.1 deliverable 5).
///
/// §5.2 calls this "the highest-risk port", and §4.1.4 exit criterion 6 gates
/// the wave on the unread divider surviving a reconnect burst, a tier change, a
/// daemon restart, and a second client marking a different channel read.
#[derive(Debug, Default)]
pub struct ReadState {
    /// Merged markers: the max across every device that has published.
    merged: std::collections::BTreeMap<String, u64>,
    /// This daemon's own client id, so a foreign blob in our slot is visible.
    client_id: String,
    /// Slot this daemon publishes into.
    slot_id: String,
    /// Whether anything has changed since the last publish, for the debounce.
    dirty: bool,
    /// Context count at which the next cap scan runs.
    ///
    /// Normally [`MAX_CONTEXTS`]; raised when a scan finds nothing evictable, so
    /// an unevictable frontier does not pay a full scan per mark forever.
    cap_check_at: usize,
}

impl ReadState {
    /// A frontier owned by `client_id`, publishing into `slot_id`.
    pub fn new(client_id: impl Into<String>, slot_id: impl Into<String>) -> Self {
        Self {
            merged: std::collections::BTreeMap::new(),
            client_id: client_id.into(),
            slot_id: slot_id.into(),
            dirty: false,
            cap_check_at: MAX_CONTEXTS,
        }
    }

    /// Merge a decrypted blob from any device, **max-wins**.
    ///
    /// Max-wins rather than last-write-wins is what makes multi-device
    /// convergent: two devices that each read a different channel both keep
    /// their progress, and neither can rewind the other. LWW would let a device
    /// that has been asleep publish a stale map and un-read everything.
    pub fn merge(&mut self, blob: &ReadStateBlob) -> bool {
        if !blob.is_valid() {
            return false;
        }
        let mut changed = false;
        for (key, marker) in &blob.contexts {
            if key.len() > MAX_CONTEXT_KEY_BYTES || *marker > MAX_MARKER {
                continue;
            }
            let slot = self.merged.entry(key.clone()).or_insert(0);
            if *marker > *slot {
                *slot = *marker;
                changed = true;
            }
        }
        if changed {
            self.enforce_context_cap();
        }
        changed
    }

    /// Advance a marker locally. **Grow-only**: a smaller value is ignored.
    ///
    /// Grow-only is what makes `msg:` markers safe. Reading an ancestor sets its
    /// own marker; it must never lower a descendant's, because the descendant is
    /// newer and the operator has not seen it.
    pub fn mark(&mut self, context: &str, marker: u64) -> bool {
        if marker > MAX_MARKER || context.len() > MAX_CONTEXT_KEY_BYTES {
            return false;
        }
        let slot = self.merged.entry(context.to_string()).or_insert(0);
        if marker > *slot {
            *slot = marker;
            self.dirty = true;
            self.enforce_context_cap();
            return true;
        }
        false
    }

    /// A context's **own** marker, without the hierarchical parent term.
    pub fn own_marker(&self, context: &str) -> Option<u64> {
        self.merged.get(context).copied()
    }

    /// The **hierarchical frontier**:
    /// `effective(ctx) = max(merged[ctx], effective(parent(ctx)))`.
    ///
    /// The recursion is what makes "mark the channel read" cover its threads
    /// without writing a marker per thread — which is the only reason the 10k
    /// context cap is livable on a busy community.
    ///
    /// The resolver is consulted at **evaluation** time, from the event graph;
    /// the link is never stored.
    pub fn effective(&self, context: &str, parent_of: ParentResolver<'_>) -> Option<u64> {
        // Bounded so a cyclic resolver — a thread whose root resolves back to
        // it through a malformed event graph — cannot hang the read path. A
        // real chain is msg → thread → channel: three levels.
        const MAX_DEPTH: usize = 8;
        let mut best = self.merged.get(context).copied();
        let mut cursor = context.to_string();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..MAX_DEPTH {
            if !seen.insert(cursor.clone()) {
                break;
            }
            let Some(parent) = parent_of(&cursor) else {
                break;
            };
            if let Some(marker) = self.merged.get(&parent).copied() {
                best = Some(best.map_or(marker, |b| b.max(marker)));
            }
            cursor = parent;
        }
        best
    }

    /// Whether an event is unread in `context`.
    ///
    /// **Non-conversational kinds are never unread** — the
    /// [`crate::timeline::is_conversational_unread_kind`] gate is applied here
    /// rather than at the caller, because a caller that forgets it produces
    /// phantom unreads that clear themselves and a divider anchored to a row
    /// nobody wrote.
    pub fn is_unread(
        &self,
        context: &str,
        kind: u32,
        created_at: u64,
        parent_of: ParentResolver<'_>,
    ) -> bool {
        if !crate::timeline::is_conversational_unread_kind(kind) {
            return false;
        }
        self.effective(context, parent_of)
            .is_none_or(|marker| created_at > marker)
    }

    /// Prune `msg:` and `thread:` markers older than the horizon.
    ///
    /// Channel markers are **never** pruned: a channel you have not opened in
    /// eight days is still a channel whose read position matters, and pruning it
    /// would resurrect every message in it as unread. Only the per-event
    /// families age out, and only because they are unbounded in number.
    pub fn prune(&mut self, now: u64) -> usize {
        let horizon = u64::from(CONTEXT_HORIZON_DAYS) * 24 * 60 * 60;
        let cutoff = now.saturating_sub(horizon);
        let before = self.merged.len();
        self.merged.retain(|key, marker| {
            let ages = is_msg_context(key) || is_thread_context(key);
            !ages || *marker >= cutoff
        });
        before - self.merged.len()
    }

    /// Split the frontier into publishable slots (§4.1.1 deliverable 5).
    ///
    /// Each slot's JSON stays under [`MAX_SLOT_BYTES`], and there are at most
    /// [`MAX_SLOTS`]. Channel keys are packed **first** so that when the budget
    /// runs out it is the per-event markers that are dropped — a lost channel
    /// marker un-reads a whole channel, a lost `msg:` marker un-reads one
    /// message.
    pub fn to_slots(&self) -> Vec<ReadStateBlob> {
        let mut channel_keys: Vec<(&String, &u64)> = Vec::new();
        let mut event_keys: Vec<(&String, &u64)> = Vec::new();
        for entry in &self.merged {
            if is_msg_context(entry.0) || is_thread_context(entry.0) {
                event_keys.push(entry);
            } else {
                channel_keys.push(entry);
            }
        }
        // Newest markers first among the ageing families: if the budget runs
        // out, the entries dropped are the ones closest to the horizon anyway.
        event_keys.sort_by(|a, b| b.1.cmp(a.1));

        let mut slots: Vec<ReadStateBlob> = Vec::new();
        let mut current = self.empty_blob();
        // Tracked incrementally rather than re-serializing the blob per entry.
        // The naive form is O(n²) over a map that is allowed to hold 10,000
        // contexts, and this runs on the publish debounce — a read-state write
        // that takes seconds of CPU stalls the socket for every other client.
        let mut current_len = blob_len(&current);
        for (key, marker) in channel_keys.into_iter().chain(event_keys) {
            let entry = entry_len(key, *marker);
            if current_len + entry > MAX_SLOT_BYTES && !current.contexts.is_empty() {
                slots.push(current);
                if slots.len() >= MAX_SLOTS {
                    return slots;
                }
                current = self.empty_blob();
                current_len = blob_len(&current);
            }
            current.contexts.insert(key.clone(), *marker);
            current_len += entry;
        }
        if !current.contexts.is_empty() || slots.is_empty() {
            slots.push(current);
        }
        slots
    }

    /// Evict the oldest ageing markers once the context cap is exceeded.
    ///
    /// [`MAX_CONTEXTS`] is validated on blobs *arriving* from other devices, but
    /// nothing bounded the local frontier: [`Self::mark`] is called once per
    /// message read, so on a busy community the map grows without limit between
    /// horizon prunes and every publish serializes all of it.
    ///
    /// Channel markers are **never** evicted, for the same reason
    /// [`Self::prune`] never ages them out: losing one un-reads a whole channel.
    /// Only the per-event families are trimmed, oldest first, which is the same
    /// order the horizon would have taken them in anyway.
    ///
    /// # Trim to a low-water mark, not to the cap
    ///
    /// Evicting exactly the excess is the obvious form and it is a performance
    /// trap: at the cap, *every subsequent* `mark` is one over, so each of them
    /// pays a full scan and sort of 10,000 entries to remove one. `mark` runs
    /// once per message read on the socket thread, so a user who has read enough
    /// to reach the cap makes every later read O(n log n) — the daemon gets
    /// slower the longer it is used, which is the shape of performance bug
    /// nobody attributes correctly.
    ///
    /// Trimming to [`CONTEXT_LOW_WATER`] instead amortizes that scan over the
    /// thousand marks it takes to climb back, at the cost of holding slightly
    /// fewer markers than the cap allows. The markers given up are the oldest
    /// per-event ones, which the 7-day horizon was going to take anyway.
    /// # And re-arm rather than rescanning when eviction cannot make progress
    ///
    /// Channel markers are not evictable, so a frontier whose contexts are
    /// mostly channels can sit above the cap with nothing to give up. Without a
    /// re-arm, *every* later `mark` rediscovers that at full scan cost and
    /// evicts nothing — an unbounded amount of work for zero progress, and the
    /// worst case is the one where the map is largest. [`Self::cap_check_at`]
    /// moves to just above the current size whenever a pass cannot reach the
    /// low-water mark, so the next scan happens only after enough new contexts
    /// have arrived to be worth one.
    fn enforce_context_cap(&mut self) {
        if self.merged.len() < self.cap_check_at {
            return;
        }
        let mut ageing: Vec<(String, u64)> = self
            .merged
            .iter()
            .filter(|(key, _)| is_msg_context(key) || is_thread_context(key))
            .map(|(key, marker)| (key.clone(), *marker))
            .collect();
        ageing.sort_by_key(|(_, marker)| *marker);

        let excess = self.merged.len().saturating_sub(CONTEXT_LOW_WATER);
        for (key, _) in ageing.into_iter().take(excess) {
            self.merged.remove(&key);
        }

        self.cap_check_at = if self.merged.len() > CONTEXT_LOW_WATER {
            // Could not reach the low-water mark: everything left is
            // unevictable. Re-arm above the current size so the next scan waits
            // for a meaningful number of new contexts rather than firing on the
            // very next mark.
            self.merged.len() + CAP_RECHECK_STRIDE
        } else {
            MAX_CONTEXTS
        };
    }

    fn empty_blob(&self) -> ReadStateBlob {
        ReadStateBlob {
            v: 1,
            client_id: self.client_id.clone(),
            contexts: std::collections::BTreeMap::new(),
        }
    }

    /// The `d` tag for slot `index`.
    ///
    /// Slot 0 keeps the configured id, so a restart republishes into the **same**
    /// `d` coordinate rather than orphaning the previous slot — an orphaned slot
    /// is read-state that still exists on the relay and is never updated again,
    /// which surfaces as markers that mysteriously stop advancing.
    pub fn slot_d_tag_for(&self, index: usize) -> String {
        if index == 0 {
            slot_d_tag(&self.slot_id)
        } else {
            slot_d_tag(&format!("{}-{index}", self.slot_id))
        }
    }

    /// Whether a foreign client is squatting this daemon's slot (§2.3).
    ///
    /// The desktop's `readStateManager.ts` rotates `slotId` on exactly this
    /// condition, and that rotation is the standing evidence that the
    /// two-daemons-on-one-identity hazard of §2.3 is real rather than
    /// theoretical.
    pub fn slot_is_squatted(&self, blob: &ReadStateBlob) -> bool {
        blob.is_valid() && blob.client_id != self.client_id
    }

    /// Rotate onto a fresh slot id after detecting a squatter.
    pub fn rotate_slot(&mut self, new_slot_id: impl Into<String>) {
        self.slot_id = new_slot_id.into();
        self.dirty = true;
    }

    /// Whether a publish is pending.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clear the pending-publish flag after a successful write.
    pub fn mark_published(&mut self) {
        self.dirty = false;
    }

    /// How many contexts are tracked.
    pub fn len(&self) -> usize {
        self.merged.len()
    }

    /// Whether nothing has been read.
    pub fn is_empty(&self) -> bool {
        self.merged.is_empty()
    }
}

fn blob_len(blob: &ReadStateBlob) -> usize {
    serde_json::to_vec(blob).map_or(usize::MAX, |v| v.len())
}

fn entry_len(key: &str, marker: u64) -> usize {
    // `"key":marker,` — the JSON cost of one entry, plus quoting and commas.
    key.len() + marker.to_string().len() + 4
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §4.1.1 deliverable 5 fixes every one of these bounds.
    #[test]
    fn bounds_match_the_design() {
        assert_eq!(MAX_SLOT_BYTES, 32 * 1024);
        assert_eq!(MAX_SLOTS, 8);
        assert_eq!(MAX_CONTEXTS, 10_000);
        assert_eq!(CONTEXT_HORIZON_DAYS, 7);
        assert_eq!(PUBLISH_DEBOUNCE_MS, 5_000);
    }

    // ── §5.2 `read-state frontier` — "the highest-risk port" ──────────────

    const CHANNEL: &str = "11111111-1111-1111-1111-111111111111";
    const NOW: u64 = 1_785_852_720;

    fn event_id(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn thread_key(n: u8) -> String {
        format!("{THREAD_PREFIX}{}", event_id(n))
    }

    fn msg_key(n: u8) -> String {
        format!("{MSG_PREFIX}{}", event_id(n))
    }

    fn state() -> ReadState {
        ReadState::new("daemon-1", "slot-a")
    }

    /// Context keys are **bare**: a channel is the raw uuid, not
    /// `channel:<uuid>`. A prefixed key would be a fourth key family the
    /// desktop does not write and would never merge with what it publishes.
    #[test]
    fn channel_contexts_are_bare_uuids() {
        assert!(!is_msg_context(CHANNEL));
        assert!(!is_thread_context(CHANNEL));
        assert!(is_thread_context(&thread_key(1)));
        assert!(is_msg_context(&msg_key(1)));
    }

    /// The two event-scoped families must be distinguishable, or the parent
    /// resolver and the horizon prune cannot tell them apart.
    #[test]
    fn the_event_scoped_families_do_not_overlap() {
        assert!(!is_thread_context(&msg_key(1)));
        assert!(!is_msg_context(&thread_key(1)));
        // A non-hex or wrong-length id is neither.
        assert!(!is_msg_context("msg:short"));
        assert!(!is_thread_context(&format!(
            "{THREAD_PREFIX}{}",
            "z".repeat(64)
        )));
        assert!(
            !is_msg_context(&format!("{MSG_PREFIX}{}", "AB".repeat(32))),
            "uppercase hex would not match what the desktop writes"
        );
    }

    /// §5.2: **hierarchical `max(ctx, parent)`.** Marking a channel read covers
    /// its threads without writing a marker per thread, which is the only
    /// reason the 10k context cap is livable.
    #[test]
    fn the_frontier_folds_in_the_parent_marker() {
        let mut state = state();
        state.mark(CHANNEL, 1_000);
        let parent_of = |key: &str| {
            if key == thread_key(1) {
                Some(CHANNEL.to_string())
            } else {
                None
            }
        };
        assert_eq!(
            state.own_marker(&thread_key(1)),
            None,
            "the thread has no marker of its own"
        );
        assert_eq!(
            state.effective(&thread_key(1), &parent_of),
            Some(1_000),
            "but it is covered by its channel"
        );
    }

    /// The fold is a **max**, so a thread read past its channel keeps its own
    /// higher marker.
    #[test]
    fn a_thread_read_past_its_channel_keeps_its_own_marker() {
        let mut state = state();
        state.mark(CHANNEL, 1_000);
        state.mark(&thread_key(1), 2_000);
        let parent_of = |key: &str| (key == thread_key(1)).then(|| CHANNEL.to_string());
        assert_eq!(state.effective(&thread_key(1), &parent_of), Some(2_000));
    }

    /// §5.2: the parent link is **graph-derived at evaluation time, never
    /// serialized**. Changing the graph changes the answer with no migration.
    #[test]
    fn the_parent_link_is_never_stored() {
        let mut state = state();
        state.mark(CHANNEL, 5_000);

        let unlinked = |_: &str| None;
        assert_eq!(state.effective(&thread_key(1), &unlinked), None);

        let linked = |key: &str| (key == thread_key(1)).then(|| CHANNEL.to_string());
        assert_eq!(state.effective(&thread_key(1), &linked), Some(5_000));

        // And nothing about the link survives into the published blob.
        let json = serde_json::to_string(&state.to_slots()[0]).unwrap();
        assert!(!json.contains("parent"), "{json}");
    }

    /// A three-level chain — `msg:` → `thread:` → channel — folds all the way.
    #[test]
    fn the_fold_walks_a_multi_level_chain() {
        let mut state = state();
        state.mark(CHANNEL, 9_000);
        let parent_of = |key: &str| {
            if key == msg_key(2) {
                Some(thread_key(1))
            } else if key == thread_key(1) {
                Some(CHANNEL.to_string())
            } else {
                None
            }
        };
        assert_eq!(state.effective(&msg_key(2), &parent_of), Some(9_000));
    }

    /// A cyclic resolver — a malformed event graph in which a thread's root
    /// resolves back to it — must not hang the read path.
    #[test]
    fn a_cyclic_parent_graph_terminates() {
        let mut state = state();
        state.mark(&thread_key(1), 7);
        let cycle = |key: &str| {
            if key == thread_key(1) {
                Some(thread_key(2))
            } else {
                Some(thread_key(1))
            }
        };
        assert_eq!(state.effective(&thread_key(1), &cycle), Some(7));
    }

    /// §5.2: "`msg:` grow-only." Reading an ancestor must never lower a
    /// descendant's marker — the descendant is newer and has not been seen.
    #[test]
    fn markers_are_grow_only() {
        let mut state = state();
        assert!(state.mark(&msg_key(1), 2_000));
        assert!(
            !state.mark(&msg_key(1), 1_000),
            "a smaller marker is ignored, not applied"
        );
        assert_eq!(state.own_marker(&msg_key(1)), Some(2_000));
    }

    /// §5.2: **max-wins merge across devices.** LWW would let a device that has
    /// been asleep publish a stale map and un-read everything.
    #[test]
    fn merging_another_device_takes_the_max_never_the_latest() {
        let mut state = state();
        state.mark(CHANNEL, 5_000);

        let stale = ReadStateBlob {
            v: 1,
            client_id: "phone".into(),
            contexts: [(CHANNEL.to_string(), 1_000)].into_iter().collect(),
        };
        assert!(!state.merge(&stale), "a stale marker changes nothing");
        assert_eq!(state.own_marker(CHANNEL), Some(5_000));

        let ahead = ReadStateBlob {
            v: 1,
            client_id: "phone".into(),
            contexts: [(CHANNEL.to_string(), 9_000)].into_iter().collect(),
        };
        assert!(state.merge(&ahead));
        assert_eq!(state.own_marker(CHANNEL), Some(9_000));
    }

    /// §4.1.4 criterion 6: "a second client marking a *different* channel
    /// read" must not disturb this one's markers.
    #[test]
    fn a_second_client_marking_another_channel_does_not_clobber_this_one() {
        let other = "22222222-2222-2222-2222-222222222222";
        let mut state = state();
        state.mark(CHANNEL, 5_000);

        let second = ReadStateBlob {
            v: 1,
            client_id: "laptop".into(),
            contexts: [(other.to_string(), 6_000)].into_iter().collect(),
        };
        state.merge(&second);
        assert_eq!(state.own_marker(CHANNEL), Some(5_000));
        assert_eq!(state.own_marker(other), Some(6_000));
    }

    /// An invalid blob is **ignored**, not partially applied: a half-parsed
    /// read-state moves some markers and not others.
    #[test]
    fn an_invalid_blob_is_ignored_entirely() {
        let mut state = state();
        for blob in [
            ReadStateBlob {
                v: 2,
                client_id: "x".into(),
                contexts: [(CHANNEL.to_string(), 9_000)].into_iter().collect(),
            },
            ReadStateBlob {
                v: 1,
                client_id: String::new(),
                contexts: [(CHANNEL.to_string(), 9_000)].into_iter().collect(),
            },
        ] {
            assert!(!state.merge(&blob));
        }
        assert_eq!(state.own_marker(CHANNEL), None);
    }

    /// One malformed *entry* filters out without discarding the blob — which
    /// carries this device's own markers too.
    #[test]
    fn a_malformed_entry_filters_without_discarding_the_blob() {
        let contexts = [
            (CHANNEL.to_string(), 9_000u64),
            ("k".repeat(MAX_CONTEXT_KEY_BYTES + 1), 9_000),
            ("overflow".to_string(), MAX_MARKER + 1),
        ]
        .into_iter()
        .collect();
        let sanitized = sanitize_contexts(contexts);
        assert_eq!(sanitized.len(), 1);
        assert_eq!(sanitized[CHANNEL], 9_000);
    }

    /// §5.2: "7-day horizon prune" — and **channels never age out**. A channel
    /// unopened for eight days is still a channel whose read position matters;
    /// pruning it would resurrect every message in it as unread.
    #[test]
    fn the_horizon_prunes_event_markers_but_never_channels() {
        let mut state = state();
        let horizon = u64::from(CONTEXT_HORIZON_DAYS) * 24 * 60 * 60;
        state.mark(CHANNEL, NOW - horizon - 10_000);
        state.mark(&msg_key(1), NOW - horizon - 1);
        state.mark(&thread_key(2), NOW - horizon - 1);
        state.mark(&msg_key(3), NOW - 100);

        assert_eq!(state.prune(NOW), 2);
        assert_eq!(
            state.own_marker(CHANNEL),
            Some(NOW - horizon - 10_000),
            "a very old channel marker survives"
        );
        assert_eq!(state.own_marker(&msg_key(1)), None);
        assert_eq!(state.own_marker(&thread_key(2)), None);
        assert_eq!(state.own_marker(&msg_key(3)), Some(NOW - 100));
    }

    /// §4.1.1 deliverable 5: "unread counting gated by
    /// `isConversationalUnreadKind` so system/job/huddle rows never create
    /// phantom unreads."
    #[test]
    fn non_conversational_kinds_never_create_unreads() {
        let state = state();
        let no_parent = |_: &str| None;
        for kind in [40099u32, 43001, 43004, 48100] {
            assert!(
                !state.is_unread(CHANNEL, kind, NOW, &no_parent),
                "kind {kind} must not create an unread"
            );
        }
        assert!(state.is_unread(CHANNEL, 9, NOW, &no_parent));
        assert!(state.is_unread(CHANNEL, 40002, NOW, &no_parent));
    }

    /// Unread is strictly *after* the marker — an event exactly at the marker
    /// has been read, or marking-read would never clear the last message.
    #[test]
    fn an_event_at_the_marker_is_read() {
        let mut state = state();
        state.mark(CHANNEL, NOW);
        let no_parent = |_: &str| None;
        assert!(!state.is_unread(CHANNEL, 9, NOW, &no_parent));
        assert!(state.is_unread(CHANNEL, 9, NOW + 1, &no_parent));
    }

    /// The unread predicate folds in the parent too, so marking a channel read
    /// clears its threads' badges without a per-thread write.
    #[test]
    fn unread_respects_the_hierarchical_frontier() {
        let mut state = state();
        state.mark(CHANNEL, NOW);
        let parent_of = |key: &str| (key == thread_key(1)).then(|| CHANNEL.to_string());
        assert!(!state.is_unread(&thread_key(1), 9, NOW - 10, &parent_of));
    }

    // ── Slot splitting ────────────────────────────────────────────────────

    #[test]
    fn a_small_frontier_publishes_as_one_slot() {
        let mut state = state();
        state.mark(CHANNEL, NOW);
        let slots = state.to_slots();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].v, 1);
        assert_eq!(slots[0].client_id, "daemon-1");
        assert!(slots[0].is_valid());
    }

    /// Every emitted slot must fit the 32 KB plaintext budget, or NIP-44
    /// encryption produces a blob the relay refuses.
    #[test]
    fn every_slot_fits_the_plaintext_budget() {
        let mut state = state();
        for n in 0..4_000u32 {
            state.mark(&format!("{n:08}-2222-2222-2222-222222222222"), NOW);
        }
        let slots = state.to_slots();
        assert!(slots.len() > 1, "this many contexts must split");
        for slot in &slots {
            let len = serde_json::to_vec(slot).unwrap().len();
            assert!(
                len <= MAX_SLOT_BYTES,
                "slot of {len} bytes exceeds the budget"
            );
        }
    }

    /// **Channel keys are packed first.** When the budget runs out it must be
    /// the per-event markers that are dropped: a lost channel marker un-reads a
    /// whole channel; a lost `msg:` marker un-reads one message.
    #[test]
    fn channel_markers_outrank_event_markers_under_pressure() {
        let mut state = state();
        for n in 0..300u32 {
            state.mark(&format!("{n:08}-3333-3333-3333-333333333333"), NOW);
        }
        for n in 0..200u8 {
            state.mark(&msg_key(n), NOW);
        }
        let first = &state.to_slots()[0];
        let channel_keys = first
            .contexts
            .keys()
            .filter(|k| !is_msg_context(k) && !is_thread_context(k))
            .count();
        assert_eq!(
            channel_keys, 300,
            "every channel marker landed before any msg: marker"
        );
    }

    /// The slot cap is hard: beyond it, entries are dropped rather than
    /// published into a ninth slot the spec does not allow.
    #[test]
    fn the_slot_count_is_capped() {
        let mut state = state();
        for n in 0..80_000u32 {
            state.mark(&format!("{n:08}-4444-4444-4444-444444444444"), NOW);
        }
        assert!(state.to_slots().len() <= MAX_SLOTS);
    }

    // ── The context cap ───────────────────────────────────────────────────

    /// The cap bounds the **local** frontier, not just incoming blobs.
    ///
    /// `mark` runs once per message read, so without this the map grows without
    /// limit between horizon prunes and every publish serializes all of it.
    #[test]
    fn the_context_cap_bounds_the_local_frontier() {
        let mut state = state();
        for n in 0..(MAX_CONTEXTS as u64 + 500) {
            state.mark(&format!("{MSG_PREFIX}{n:064x}"), NOW + n);
        }
        assert!(
            state.len() <= MAX_CONTEXTS,
            "the frontier grew to {} contexts",
            state.len()
        );
    }

    /// Eviction takes the **oldest** ageing markers, which is the same order the
    /// 7-day horizon would have taken them in.
    #[test]
    fn the_cap_evicts_the_oldest_ageing_markers_first() {
        let mut state = state();
        for n in 0..(MAX_CONTEXTS as u64 + 100) {
            // Marker value ascends with n, so low n is oldest.
            state.mark(&format!("{MSG_PREFIX}{n:064x}"), NOW + n);
        }
        assert_eq!(
            state.own_marker(&format!("{MSG_PREFIX}{:064x}", 0)),
            None,
            "the oldest marker was evicted"
        );
        let newest = MAX_CONTEXTS as u64 + 99;
        assert!(
            state
                .own_marker(&format!("{MSG_PREFIX}{newest:064x}"))
                .is_some(),
            "the newest marker survived"
        );
    }

    /// **Channel markers are never evicted**, for the same reason the horizon
    /// never ages them out: losing one un-reads a whole channel.
    #[test]
    fn the_cap_never_evicts_a_channel_marker() {
        let mut state = state();
        state.mark(CHANNEL, NOW);
        for n in 0..(MAX_CONTEXTS as u64 + 500) {
            state.mark(&format!("{MSG_PREFIX}{n:064x}"), NOW + n);
        }
        assert_eq!(
            state.own_marker(CHANNEL),
            Some(NOW),
            "a channel marker must survive any amount of per-message churn"
        );
    }

    /// An **unevictable** frontier must not pay a full scan per mark forever.
    ///
    /// Regression test for a real performance bug: trimming to exactly the cap
    /// left every subsequent `mark` one-over, and a frontier made of
    /// non-evictable channel markers rediscovered "nothing to evict" at full
    /// scan-and-sort cost on every read. The daemon got slower the longer it was
    /// used, which is the shape of bug nobody attributes correctly.
    ///
    /// Asserted as a **time bound**, because that is the actual property: an
    /// implementation that scanned per mark takes minutes here, and one that
    /// re-arms takes well under a second.
    #[test]
    fn an_unevictable_frontier_does_not_rescan_on_every_mark() {
        let mut state = state();
        let start = std::time::Instant::now();
        // Channel keys only: nothing here is ever evictable.
        for n in 0..(MAX_CONTEXTS as u64 + 20_000) {
            state.mark(&format!("{n:08}-5555-5555-5555-555555555555"), NOW);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "marking past the cap with nothing evictable took {elapsed:?}; \
             the cap scan is running per mark instead of re-arming"
        );
        assert!(
            state.len() > MAX_CONTEXTS,
            "and nothing was wrongly evicted"
        );
    }

    /// Slot 0 keeps the configured id, so a restart republishes into the same
    /// `d` coordinate rather than orphaning the previous slot.
    #[test]
    fn slot_zero_keeps_the_configured_d_coordinate() {
        let state = state();
        assert_eq!(state.slot_d_tag_for(0), "read-state:slot-a");
        assert_eq!(state.slot_d_tag_for(1), "read-state:slot-a-1");
        assert!(is_read_state_d_tag(&state.slot_d_tag_for(0)));
        assert!(is_read_state_d_tag(&state.slot_d_tag_for(1)));
    }

    #[test]
    fn a_non_read_state_d_tag_is_rejected() {
        assert!(!is_read_state_d_tag("something-else"));
        assert!(!is_read_state_d_tag(READ_STATE_D_TAG_PREFIX));
        assert!(!is_read_state_d_tag(&slot_d_tag(&"x".repeat(65))));
    }

    /// §2.3: the desktop rotates `slotId` when another `client_id` squats its
    /// `d` tag, and that rotation is the standing evidence that the
    /// two-daemons-on-one-identity hazard is real.
    #[test]
    fn a_foreign_client_id_in_our_slot_is_detected() {
        let mut state = state();
        let ours = ReadStateBlob {
            v: 1,
            client_id: "daemon-1".into(),
            contexts: Default::default(),
        };
        let theirs = ReadStateBlob {
            v: 1,
            client_id: "some-other-daemon".into(),
            contexts: Default::default(),
        };
        assert!(!state.slot_is_squatted(&ours));
        assert!(state.slot_is_squatted(&theirs));

        state.rotate_slot("slot-b");
        assert_eq!(state.slot_d_tag_for(0), "read-state:slot-b");
    }

    /// The 5 s debounce needs a dirty flag, and only a *real* advance sets it —
    /// otherwise a re-delivered marker after a reconnect triggers a publish.
    #[test]
    fn only_a_real_advance_marks_the_state_dirty() {
        let mut state = state();
        assert!(!state.is_dirty());
        state.mark(CHANNEL, 1_000);
        assert!(state.is_dirty());
        state.mark_published();
        assert!(!state.is_dirty());
        state.mark(CHANNEL, 500);
        assert!(!state.is_dirty(), "a stale marker is not a change");
    }
}
