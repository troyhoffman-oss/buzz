//! Timeline fetch — the NIP-CW server-assembled window.
//!
//! Implements `DESIGN.md` §2.4 [D-10] and Wave-1 daemon deliverable 4 (§4.1.1).
//!
//! An earlier draft specified a two-query fetch (content kinds by time window,
//! aux kinds by `#e`) and called it "verbatim". It is not verbatim — it is the
//! *pre*-NIP-CW desktop path, and reimplementing it reintroduces exactly the
//! bugs NIP-CW exists to remove. The current desktop sends `top_level: true`,
//! `include_summaries: true`, `include_aux: true`, plus `(until, before_id)`
//! (`desktop/src-tauri/src/commands/channel_window.rs`). Three things the
//! two-query plan loses:
//!
//! - A plain `kinds` + `#h` filter **cannot express "not a reply"**, so `limit`
//!   counts raw events: a page of 50 may contain 3 top-level rows or 50.
//! - `39006` carries the authoritative `has_more`. NIP-CW §Client Behavior is
//!   explicit — *"A client MUST NOT stop paging on row count"*; an
//!   exact-multiple final page returns `limit` rows with `has_more: false`. The
//!   obvious implementation of the `{next, has_more}` contract (short page =
//!   done) is the precise bug the NIP forbids.
//! - Thread summaries (`39005`) arrive as relay-signed overlays with the
//!   window, and are the cheap source for §3.1's `⤷ 4` reply count.
//!
//! # The degradation branch is also Wave 1
//!
//! No valid `39006` (extension-unaware relay, or a strict parser rejecting the
//! filter) → reissue a clean standard filter with the extension keys removed
//! and assemble threads client-side, which is what `auxBackfill.ts` still does.
//! **Downgrade is a decision, not a fallback that happens by accident** — and a
//! downgrade that has never run is a downgrade that does not work, which is why
//! it ships in the same wave.

use serde::{Deserialize, Serialize};

use crate::cursor::Cursor;
use crate::error::{DaemonError, Result};

/// The timeline kind set, **verbatim** from
/// `desktop/src-tauri/src/commands/channel_window.rs`'s `TIMELINE_KINDS`.
///
/// §3.1: non-conversational kinds (40099 system, 43001–43006 job, 48100 huddle
/// started) render as their own dimmed rows and are excluded from the unread
/// pill, per `isConversationalUnreadKind`. Note **48100 only** — 48101–48103 are
/// neither fetched nor rendered in Wave 1. An earlier draft rendered them while
/// fetching only 48100, which is unimplementable as written and made a §5.2
/// test case assert on a path that cannot occur. They arrive with the read-only
/// huddle surface in Wave 4.
pub const TIMELINE_KINDS: [u32; 11] = [
    9,     // channel message
    40002, // rich message
    40008, // diff message — §3.1 gives these their own row
    40099, // system
    43001,
    43002,
    43003,
    43004,
    43005,
    43006,                                // job
    buzz_core::kind::KIND_HUDDLE_STARTED, // 48100 — and only 48100
];

/// The **conversational** subset of [`TIMELINE_KINDS`].
///
/// §4.1.1 deliverable 5: "unread counting gated by `isConversationalUnreadKind`
/// so system/job/huddle rows never create phantom unreads." A job-lifecycle
/// event or a huddle-started row is a thing that *happened*, not a thing
/// somebody said to you — counting it produces an unread badge that clears
/// itself and a divider anchored to a row nobody wrote.
pub const CONVERSATIONAL_KINDS: [u32; 3] = [
    9,     // channel message
    40002, // rich message
    40008, // diff message
];

/// Whether an event kind creates unread state.
///
/// The gate of §4.1.1 deliverable 5. Deliberately a whitelist rather than a
/// blacklist: a new non-conversational kind added upstream defaults to "does
/// not create unreads", which fails toward a quiet badge rather than a phantom
/// one.
pub fn is_conversational_unread_kind(kind: u32) -> bool {
    CONVERSATIONAL_KINDS.contains(&kind)
}

/// Relay-signed thread summary overlay. Source of §3.1's `⤷ 4` reply count.
pub const KIND_THREAD_SUMMARY: u32 = buzz_core::kind::KIND_THREAD_SUMMARY;

/// Relay-signed window-bounds overlay. **The sole exhaustion authority** —
/// see [`WindowBounds::has_more`].
pub const KIND_WINDOW_BOUNDS: u32 = buzz_core::kind::KIND_WINDOW_BOUNDS;

/// The `kind:39006` overlay that terminates (or continues) paging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds {
    /// Authoritative exhaustion signal. **Row count is never used** —
    /// NIP-CW §Client Behavior: *"A client MUST NOT stop paging on row count."*
    pub has_more: bool,
    /// Cursor for the next page; `None` exactly when `has_more` is false.
    pub next_cursor: Option<String>,
    /// The `d`-tag binding echoed back from the request, used by
    /// [`check_bounds_integrity`].
    pub binding: String,
}

/// Build the NIP-CW window filter (§4.1.1 deliverable 4).
///
/// Mirrors `build_channel_window_filter` in the desktop, including the
/// composite `(until, before_id)` echoed from the previous page's `39006`.
///
/// `kinds` is always set — not as a search rule but as the **global daemon
/// invariant** of §2.4: the relay's `p_gated_filters_authorized` gate refuses a
/// filter that can match a `P_GATED_KIND`, and a filter with no `kinds` can
/// match everything.
pub fn build_window_filter(
    channel_id: &str,
    limit: u32,
    cursor: Option<&Cursor>,
) -> serde_json::Value {
    let mut filter = serde_json::Map::new();
    filter.insert("#h".into(), serde_json::json!([channel_id]));
    filter.insert("kinds".into(), serde_json::json!(TIMELINE_KINDS));
    filter.insert("limit".into(), serde_json::json!(limit));
    filter.insert("top_level".into(), serde_json::json!(true));
    filter.insert("include_summaries".into(), serde_json::json!(true));
    filter.insert("include_aux".into(), serde_json::json!(true));
    if let Some(cursor) = cursor {
        filter.insert("until".into(), serde_json::json!(cursor.until));
        filter.insert("before_id".into(), serde_json::json!(cursor.before_id));
    }
    serde_json::Value::Object(filter)
}

/// The degraded filter used when a relay serves no valid `39006` ([D-10]).
///
/// A clean *standard* filter with the three **NIP-CW window** keys removed —
/// `top_level`, `include_summaries`, `include_aux`. Threads are then assembled
/// client-side and aux is fetched by `#e` over loaded ids, which is what
/// `auxBackfill.ts` still does.
///
/// # `before_id` stays, and dropping it was a defect
///
/// An earlier revision of this function emitted `until` alone, on the reading
/// that "the extension keys" included the composite cursor. It does not, and the
/// distinction is load-bearing in two directions:
///
/// - **`before_id` is not a window key.** The relay accepts it on the ordinary
///   `/query` path (`crates/buzz-relay/src/api/bridge.rs:1245`), gated only on
///   `until` being set, and `buzz-cli`'s own `advance_query_cursor` sets both
///   fields for every paginated read. It is the *general* bridge cursor, not
///   part of the NIP-CW window grammar.
/// - **`until` alone can livelock the walk.** NIP-01's `until` is inclusive, so
///   a timestamp-only cursor re-requests every event sharing that second. Dedup
///   absorbs the duplicates — but if a whole page shares one `created_at`, the
///   next request returns *the same page*, and paging never advances. The
///   degradation branch would hang on exactly the dense-second traffic an agent
///   channel produces, and it would hang only there, which is the worst place
///   for a bug to live.
///
/// A relay strict enough to reject the unknown key rejects the whole filter
/// rather than silently half-applying it, which surfaces as an error the
/// operator can see. A relay that merely ignores it degrades to `until`-only —
/// no worse than emitting `until` alone, and better everywhere else.
pub fn build_downgraded_filter(
    channel_id: &str,
    limit: u32,
    cursor: Option<&Cursor>,
) -> serde_json::Value {
    let mut filter = serde_json::Map::new();
    filter.insert("#h".into(), serde_json::json!([channel_id]));
    filter.insert("kinds".into(), serde_json::json!(TIMELINE_KINDS));
    filter.insert("limit".into(), serde_json::json!(limit));
    if let Some(cursor) = cursor {
        filter.insert("until".into(), serde_json::json!(cursor.until));
        filter.insert("before_id".into(), serde_json::json!(cursor.before_id));
    }
    serde_json::Value::Object(filter)
}

/// Bounds-integrity checks per NIP-CW §Client Behavior step 5 ([D-10]).
///
/// Exactly one `39006`; its `d`-tag binding echoes the request cursor; content
/// parses; and `has_more = true ⇔ next_cursor ≠ null`. Anything else means
/// **discard the page and retry, never guess**.
pub fn check_bounds_integrity(bounds: &[WindowBounds], expected_binding: &str) -> Result<()> {
    match bounds.len() {
        1 => {}
        n => {
            return Err(DaemonError::WindowIntegrity(format!(
                "expected exactly one kind:39006 overlay, got {n}"
            )))
        }
    }
    let bounds = &bounds[0];
    if bounds.binding != expected_binding {
        return Err(DaemonError::WindowIntegrity(format!(
            "39006 binding {:?} does not echo the request cursor {:?}",
            bounds.binding, expected_binding
        )));
    }
    if bounds.has_more != bounds.next_cursor.is_some() {
        return Err(DaemonError::WindowIntegrity(
            "has_more must hold exactly when next_cursor is present".into(),
        ));
    }
    Ok(())
}

/// Whether paging continues. **The only** exhaustion authority ([D-10]).
///
/// Deliberately takes no row count: the signature is what makes "short page =
/// done" — the precise bug NIP-CW forbids — unrepresentable rather than merely
/// discouraged.
pub fn should_continue_paging(bounds: &WindowBounds) -> bool {
    bounds.has_more
}

/// Auxiliary overlay kinds — metadata about rows, never rows themselves.
///
/// Verbatim from `CHANNEL_AUX_EVENT_KINDS`
/// (`desktop/src/shared/constants/kinds.ts:112`). [D-10]: "overlays are
/// metadata and never render as rows or feed cursor math." An aux event
/// counted as a row would both fabricate a timeline entry and corrupt the
/// cursor, because the cursor is derived from the last *row*.
pub const AUX_KINDS: [u32; 4] = [
    5,     // NIP-09 deletion
    7,     // NIP-25 reaction
    9005,  // NIP-29 / Buzz-native deletion
    40003, // message edit
];

/// Whether a kind is an auxiliary overlay.
pub fn is_aux_kind(kind: u32) -> bool {
    AUX_KINDS.contains(&kind)
}

/// Whether a kind renders as its own timeline row.
pub fn is_content_kind(kind: u32) -> bool {
    TIMELINE_KINDS.contains(&kind)
}

/// A relay-signed `kind:39005` thread summary — §3.1's `⤷ 4` reply count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    /// Direct replies to the root.
    pub reply_count: u32,
    /// Replies at any depth.
    pub descendant_count: u32,
    /// `created_at` of the newest reply, when there is one.
    pub last_reply_at: Option<u64>,
    /// Distinct participant pubkeys.
    pub participants: Vec<String>,
}

/// One assembled timeline row: a content event plus its thread overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineRow {
    /// The content event, verbatim.
    pub event: serde_json::Value,
    /// The `39005` overlay bound to this row, when the relay sent one.
    pub thread: Option<ThreadSummary>,
    /// Whether this row is **non-conversational** — system, job, or
    /// huddle-started (§3.1), which render as their own dimmed rows.
    ///
    /// Carried on the row rather than left for the client to derive, because
    /// deriving it means knowing which kinds are conversational, and kinds are
    /// daemon vocabulary (§6.4 — the TUI's `check-boundary.sh` fails the build
    /// on a bare kind integer in `src/`). Without it a 40099 `dm_created`
    /// renders as its raw JSON payload in the middle of a conversation, which
    /// is what the M3 live walk caught.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
}

/// How a page was assembled — the honest answer to "did the extension work?".
///
/// [D-10]: "Downgrade is a decision, not a fallback that happens by accident."
/// Recording it on the page makes the decision observable on `GET /daemon` and
/// in a support conversation, rather than something you infer from the shape of
/// what came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    /// The relay served a valid `39006`; exhaustion is authoritative.
    Nipcw,
    /// No valid `39006`. Threads assembled client-side, aux fetched by `#e`,
    /// and exhaustion inferred from the row count — which is only sound because
    /// there is no server-assembled window to be wrong about.
    Downgraded,
}

/// One assembled page of a channel timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPage {
    /// Content rows in wire order.
    pub rows: Vec<TimelineRow>,
    /// Aux overlays that arrived with the page. Metadata: never rows, never
    /// cursor input.
    pub aux: Vec<serde_json::Value>,
    /// Cursor for the next page, or `None` when exhausted.
    pub next_cursor: Option<Cursor>,
    /// Whether more pages exist.
    pub has_more: bool,
    /// How this page was assembled.
    pub mode: WindowMode,
}

/// The `d`-tag binding a `39006` must echo.
///
/// Verbatim from `expectedBoundsKey`
/// (`desktop/src/features/messages/lib/channelWindowResponse.ts:71`):
/// `<channel>:<until>:<before_id>` lowercased, or `<channel>:head` for the
/// first page. Lowercasing matters — an id echoed in a different case would
/// fail an exact comparison and discard a page that was in fact correct.
pub fn expected_bounds_binding(channel_id: &str, cursor: Option<&Cursor>) -> String {
    match cursor {
        Some(cursor) => format!(
            "{}:{}:{}",
            channel_id.to_lowercase(),
            cursor.until,
            cursor.before_id.to_lowercase()
        ),
        None => format!("{}:head", channel_id.to_lowercase()),
    }
}

/// First tag value with the given name.
fn tag_value<'a>(event: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    event
        .get("tags")?
        .as_array()?
        .iter()
        .find(|tag| tag.get(0).and_then(serde_json::Value::as_str) == Some(name))
        .and_then(|tag| tag.get(1))
        .and_then(serde_json::Value::as_str)
}

fn event_kind(event: &serde_json::Value) -> Option<u32> {
    event
        .get("kind")
        .and_then(serde_json::Value::as_u64)
        .map(|k| k as u32)
}

/// Parse the `39006` overlay out of a raw event.
fn parse_bounds(event: &serde_json::Value) -> Result<WindowBounds> {
    let binding = tag_value(event, "d")
        .ok_or_else(|| DaemonError::WindowIntegrity("39006 carries no d tag".into()))?
        .to_string();
    let content = event
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DaemonError::WindowIntegrity("39006 has no content".into()))?;
    let payload: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| DaemonError::WindowIntegrity(format!("39006 content is not JSON: {e}")))?;

    let has_more = payload
        .get("has_more")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| DaemonError::WindowIntegrity("39006 has no has_more".into()))?;
    let next_cursor = match payload.get("next_cursor") {
        None | Some(serde_json::Value::Null) => None,
        Some(cursor) => {
            let until = cursor
                .get("created_at")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    DaemonError::WindowIntegrity("next_cursor has no created_at".into())
                })?;
            let before_id = cursor
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| DaemonError::WindowIntegrity("next_cursor has no id".into()))?;
            Some(Cursor {
                until,
                before_id: before_id.to_string(),
            })
        }
    };

    Ok(WindowBounds {
        has_more,
        // Encoded rather than stored raw so the field that leaves the daemon is
        // always the [D-6] `c1.` form. A wire cursor and a client cursor with
        // two different shapes is how a client ends up parsing one.
        next_cursor: next_cursor.as_ref().map(Cursor::encode),
        binding,
    })
}

/// Parse a `39005` overlay into `(root_id, summary)`.
///
/// A malformed overlay yields `None` rather than an error: it costs one badge
/// refresh, and failing the whole page over a reply count would discard a
/// timeline the operator can otherwise read. That is the same trade
/// `parseLiveThreadSummary` makes in the desktop.
fn parse_thread_summary(event: &serde_json::Value) -> Option<(String, ThreadSummary)> {
    let root_id = tag_value(event, "e")?.to_string();
    let content = event.get("content").and_then(serde_json::Value::as_str)?;
    let payload: serde_json::Value = serde_json::from_str(content).ok()?;
    Some((
        root_id,
        ThreadSummary {
            reply_count: payload
                .get("reply_count")
                .and_then(serde_json::Value::as_u64)? as u32,
            descendant_count: payload
                .get("descendant_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32,
            last_reply_at: payload
                .get("last_reply_at")
                .and_then(serde_json::Value::as_u64),
            participants: payload
                .get("participants")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        },
    ))
}

/// Partition a flat `/query` response into a NIP-CW page ([D-10]).
///
/// Ported from `parseChannelWindowResponse`
/// (`desktop/src/features/messages/lib/channelWindowResponse.ts:82`). Every
/// integrity failure **discards the page** — the caller retries or downgrades,
/// and never guesses.
pub fn parse_window_response(
    events: &[serde_json::Value],
    channel_id: &str,
    cursor: Option<&Cursor>,
) -> Result<WindowPage> {
    let bounds_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| event_kind(e) == Some(KIND_WINDOW_BOUNDS))
        .collect();

    let parsed: Vec<WindowBounds> = bounds_events
        .iter()
        .map(|e| parse_bounds(e))
        .collect::<Result<_>>()?;
    check_bounds_integrity(&parsed, &expected_bounds_binding(channel_id, cursor))?;
    let bounds = &parsed[0];

    let mut summaries: std::collections::BTreeMap<String, ThreadSummary> =
        std::collections::BTreeMap::new();
    for event in events {
        if event_kind(event) == Some(KIND_THREAD_SUMMARY) {
            if let Some((root_id, summary)) = parse_thread_summary(event) {
                summaries.insert(root_id, summary);
            }
        }
    }

    let rows = events
        .iter()
        .filter(|e| event_kind(e).is_some_and(is_content_kind))
        .map(|event| {
            let id = event.get("id").and_then(serde_json::Value::as_str);
            TimelineRow {
                thread: id.and_then(|id| summaries.get(id).cloned()),
                system: event_kind(event).is_some_and(|k| !is_conversational_unread_kind(k)),
                event: event.clone(),
            }
        })
        .collect();

    let aux = events
        .iter()
        .filter(|e| event_kind(e).is_some_and(is_aux_kind))
        .cloned()
        .collect();

    Ok(WindowPage {
        rows,
        aux,
        next_cursor: bounds
            .next_cursor
            .as_deref()
            .map(Cursor::decode)
            .transpose()?,
        has_more: bounds.has_more,
        mode: WindowMode::Nipcw,
    })
}

/// Assemble a page from a relay that served no valid `39006` ([D-10]).
///
/// The degradation branch, and it ships in the same wave **because a downgrade
/// that has never run is a downgrade that does not work**. Three differences
/// from the NIP-CW path, each forced by what the standard filter cannot say:
///
/// 1. **Threads are assembled client-side.** No `39005` overlays arrive, so
///    reply counts are derived from the `#e`-tagged replies among the rows.
/// 2. **`limit` counts raw events, not top-level rows**, because a standard
///    filter cannot express "not a reply". A page of 50 may hold 3 rows.
/// 3. **Exhaustion is inferred from a short page.** This is the exact
///    inference NIP-CW forbids — but only *because* NIP-CW has a
///    server-assembled window whose row count is not the page size. With no
///    window, a short page really is the end, and it is the only signal there
///    is. Marking the page [`WindowMode::Downgraded`] is what keeps the two
///    rules from being confused.
pub fn assemble_downgraded_page(events: &[serde_json::Value], requested_limit: u32) -> WindowPage {
    let rows: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| event_kind(e).is_some_and(is_content_kind))
        .collect();

    // Client-side thread assembly: count replies per root over the events we
    // actually have. Deliberately a *lower bound* — a root whose replies fall
    // outside this window counts low rather than reporting a confident wrong
    // number, which is the same "null ≠ 0" discipline §5.2 applies to metrics.
    let mut reply_counts: std::collections::BTreeMap<String, u32> =
        std::collections::BTreeMap::new();
    let mut participants: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut last_reply: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for event in &rows {
        let Some(root) = reply_root(event) else {
            continue;
        };
        *reply_counts.entry(root.clone()).or_insert(0) += 1;
        if let Some(pubkey) = event.get("pubkey").and_then(serde_json::Value::as_str) {
            participants
                .entry(root.clone())
                .or_default()
                .insert(pubkey.to_string());
        }
        if let Some(created_at) = event.get("created_at").and_then(serde_json::Value::as_u64) {
            let slot = last_reply.entry(root).or_insert(created_at);
            *slot = (*slot).max(created_at);
        }
    }

    // Only *top-level* events become rows: the standard filter could not say
    // `top_level: true`, so replies came back mixed in with roots and must be
    // partitioned here instead.
    let top_level: Vec<&serde_json::Value> = rows
        .iter()
        .copied()
        .filter(|e| reply_root(e).is_none())
        .collect();

    let assembled = top_level
        .iter()
        .map(|event| {
            let id = event.get("id").and_then(serde_json::Value::as_str);
            let thread = id.and_then(|id| {
                reply_counts.get(id).map(|count| ThreadSummary {
                    reply_count: *count,
                    // Descendant depth is not derivable from a flat window
                    // without every intermediate reply, so it reports the
                    // direct count rather than a fabricated deeper number.
                    descendant_count: *count,
                    last_reply_at: last_reply.get(id).copied(),
                    participants: participants
                        .get(id)
                        .map(|set| set.iter().cloned().collect())
                        .unwrap_or_default(),
                })
            });
            TimelineRow {
                thread,
                system: event_kind(event).is_some_and(|k| !is_conversational_unread_kind(k)),
                event: (*event).clone(),
            }
        })
        .collect();

    let aux = events
        .iter()
        .filter(|e| event_kind(e).is_some_and(is_aux_kind))
        .cloned()
        .collect();

    // A short page ends the walk; a full one continues from the last *raw*
    // event, because the relay paged over raw events and the cursor must speak
    // the same language the relay does.
    let has_more = events.len() >= requested_limit as usize;
    let next_cursor = if has_more {
        events.last().and_then(|last| {
            Some(Cursor {
                until: last.get("created_at").and_then(serde_json::Value::as_u64)?,
                before_id: last
                    .get("id")
                    .and_then(serde_json::Value::as_str)?
                    .to_string(),
            })
        })
    } else {
        None
    };

    WindowPage {
        rows: assembled,
        aux,
        // `has_more` must agree with the cursor here too: a full page whose last
        // event carries no usable id has nowhere to continue from, and claiming
        // otherwise would loop forever on the same page.
        has_more: has_more && next_cursor.is_some(),
        next_cursor,
        mode: WindowMode::Downgraded,
    }
}

/// The NIP-10 root this event replies to, or `None` when it is top-level.
///
/// Prefers an explicit `["e", id, "", "root"]` marker; falls back to the first
/// `e` tag, which is what an unmarked NIP-10 reply looks like.
pub fn reply_root(event: &serde_json::Value) -> Option<String> {
    let tags = event.get("tags")?.as_array()?;
    let marked = tags.iter().find(|tag| {
        tag.get(0).and_then(serde_json::Value::as_str) == Some("e")
            && tag.get(3).and_then(serde_json::Value::as_str) == Some("root")
    });
    let tag = marked.or_else(|| {
        tags.iter()
            .find(|tag| tag.get(0).and_then(serde_json::Value::as_str) == Some("e"))
    })?;
    tag.get(1)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Build the aux backfill filter for the degradation branch ([D-10]).
///
/// Aux by `#e` over the loaded ids, which is what `auxBackfill.ts` still does.
/// `kinds` is explicit, per §2.4.
pub fn build_aux_backfill_filter(message_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "kinds": AUX_KINDS,
        "#e": message_ids,
        // Bounded on purpose. Aux is *unbounded per row* — a single popular
        // message can carry thousands of reactions — so a filter with no
        // `limit` lets one `GET /message/{id}/reaction` pull an arbitrarily
        // large response into a handler that then groups it under the daemon's
        // single mutex. The cap is generous relative to any message a human
        // reads and small enough that the worst case is bounded.
        "limit": AUX_BACKFILL_LIMIT,
    })
}

/// Cap on one aux backfill response.
pub const AUX_BACKFILL_LIMIT: u32 = 500;

/// Build the thread-resolution filter for `GET /message/{id}/thread`.
///
/// Threads are `#e`-scoped rather than `#h`-scoped: a thread is defined by its
/// root event, and the channel it lives in is a property of the root, not a
/// second constraint. Content kinds only — a reaction on a reply is aux and is
/// backfilled separately.
pub fn build_thread_filter(root_event_id: &str, limit: u32) -> serde_json::Value {
    serde_json::json!({
        "kinds": TIMELINE_KINDS,
        "#e": [root_event_id],
        "limit": limit,
    })
}

/// Order a fetched thread into a stable, depth-aware sequence.
///
/// Sorted by `created_at` then by id. The id tiebreak is what makes the order
/// **total**: two replies posted in the same second would otherwise sort
/// differently between two clients, and a thread that renders in two orders is
/// a thread whose "third reply" means nothing in a conversation.
pub fn sort_thread(mut events: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    events.sort_by(|a, b| {
        let ts = |e: &serde_json::Value| {
            e.get("created_at")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        let id = |e: &serde_json::Value| {
            e.get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        ts(a).cmp(&ts(b)).then_with(|| id(a).cmp(&id(b)))
    });
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §3.1: `TIMELINE_KINDS` verbatim — **48100 only**.
    #[test]
    fn timeline_kinds_match_the_desktop_verbatim() {
        assert_eq!(
            TIMELINE_KINDS,
            [9, 40002, 40008, 40099, 43001, 43002, 43003, 43004, 43005, 43006, 48100]
        );
        for absent in [48101u32, 48102, 48103] {
            assert!(
                !TIMELINE_KINDS.contains(&absent),
                "{absent} is Wave 4, not Wave 1"
            );
        }
    }

    /// **M3 regression.** A non-conversational row must say so on the wire.
    ///
    /// §3.1 gives system/job/huddle kinds "their own dimmed rows", and the TUI's
    /// renderer implements exactly that — but it can only act on a flag,
    /// because deriving one would mean knowing which kinds are conversational,
    /// and kinds are daemon vocabulary (§6.4; the TUI's `check-boundary.sh`
    /// fails the build on a bare kind integer in `src/`).
    ///
    /// Without the flag the M3 live walk rendered a 40099 `dm_created` as its
    /// raw JSON payload — `{"actor":"5282…","participants":[…]}` — wrapped
    /// across three lines in the middle of a conversation.
    #[test]
    fn a_non_conversational_row_is_marked_system() {
        let system = serde_json::json!({
            "id": id(9),
            "kind": 40099,
            "pubkey": "aa".repeat(32),
            "created_at": 1_700_000_100u64,
            "content": r#"{"type":"dm_created"}"#,
            "tags": [["h", CHANNEL]],
        });
        let page = assemble_downgraded_page(&[message(1, 1_700_000_090), system], 50);
        assert_eq!(page.rows.len(), 2);
        assert!(
            !page.rows[0].system,
            "a kind-9 message is conversational and must not be dimmed"
        );
        assert!(
            page.rows[1].system,
            "a 40099 is a system row; without this it renders as raw JSON"
        );
    }

    /// §2.4's global invariant: no filter leaves the daemon without `kinds`.
    #[test]
    fn both_filters_always_carry_explicit_kinds() {
        for filter in [
            build_window_filter("chan", 50, None),
            build_downgraded_filter("chan", 50, None),
        ] {
            assert!(filter.get("kinds").is_some(), "{filter}");
        }
    }

    /// [D-10]: the window filter carries the three extension keys; the
    /// downgraded one carries none of them.
    #[test]
    fn window_filter_carries_the_extension_keys_and_downgrade_does_not() {
        let window = build_window_filter("chan", 50, None);
        for key in ["top_level", "include_summaries", "include_aux"] {
            assert_eq!(window.get(key), Some(&serde_json::json!(true)), "{key}");
        }
        let downgraded = build_downgraded_filter("chan", 50, None);
        for key in ["top_level", "include_summaries", "include_aux"] {
            assert!(downgraded.get(key).is_none(), "{key} leaked into downgrade");
        }
    }

    /// `before_id` is the **general bridge cursor**, not a NIP-CW window key,
    /// so the downgrade keeps it.
    ///
    /// The relay accepts it on the ordinary `/query` path
    /// (`crates/buzz-relay/src/api/bridge.rs:1245`) and `buzz-cli`'s own
    /// `advance_query_cursor` sets both fields for every paginated read.
    /// Dropping it would leave `until` alone — and NIP-01's `until` is
    /// **inclusive**, so a page whose events all share one `created_at`
    /// re-requests itself forever. The degradation branch would livelock on
    /// exactly the dense-second traffic an agent channel produces.
    #[test]
    fn the_downgrade_keeps_the_composite_cursor() {
        let cursor = Cursor {
            until: 1_700_000_000,
            before_id: "cd".repeat(32),
        };
        let downgraded = build_downgraded_filter("chan", 50, Some(&cursor));
        assert_eq!(downgraded["until"], serde_json::json!(cursor.until));
        assert_eq!(
            downgraded["before_id"],
            serde_json::json!(cursor.before_id),
            "until alone re-requests every event sharing that second"
        );
    }

    /// The head request carries no cursor at all in either branch — `before_id`
    /// without `until` is a `400` at the bridge, not a head request.
    #[test]
    fn a_head_request_carries_neither_cursor_field() {
        for filter in [
            build_window_filter("chan", 50, None),
            build_downgraded_filter("chan", 50, None),
        ] {
            assert!(filter.get("until").is_none(), "{filter}");
            assert!(filter.get("before_id").is_none(), "{filter}");
        }
    }

    /// [D-10]: the composite cursor is echoed as `(until, before_id)`.
    #[test]
    fn cursor_is_echoed_as_a_composite() {
        let cursor = Cursor {
            until: 1_700_000_000,
            before_id: "cd".repeat(32),
        };
        let filter = build_window_filter("chan", 50, Some(&cursor));
        assert_eq!(filter["until"], serde_json::json!(cursor.until));
        assert_eq!(filter["before_id"], serde_json::json!(cursor.before_id));
    }

    /// §5.2: "a **full** page with `has_more: false` terminates; row count is
    /// never used as an exhaustion signal."
    #[test]
    fn a_full_page_with_has_more_false_terminates() {
        let bounds = WindowBounds {
            has_more: false,
            next_cursor: None,
            binding: "b".into(),
        };
        assert!(!should_continue_paging(&bounds));
    }

    #[test]
    fn a_page_with_has_more_true_continues() {
        let bounds = WindowBounds {
            has_more: true,
            next_cursor: Some("c1.abc".into()),
            binding: "b".into(),
        };
        assert!(should_continue_paging(&bounds));
    }

    /// [D-10]: exactly one `39006`. Missing or duplicated discards the page.
    #[test]
    fn missing_or_duplicated_bounds_discards_the_page() {
        assert_eq!(
            check_bounds_integrity(&[], "b").unwrap_err().code(),
            "window_integrity"
        );
        let one = WindowBounds {
            has_more: false,
            next_cursor: None,
            binding: "b".into(),
        };
        assert!(check_bounds_integrity(&[one.clone(), one], "b").is_err());
    }

    /// [D-10]: the `d`-tag binding must echo the request cursor.
    #[test]
    fn mis_bound_overlay_discards_the_page() {
        let bounds = WindowBounds {
            has_more: false,
            next_cursor: None,
            binding: "someone-elses-page".into(),
        };
        assert!(check_bounds_integrity(&[bounds], "mine").is_err());
    }

    /// [D-10]: `has_more = true ⇔ next_cursor ≠ null`; a violation discards.
    #[test]
    fn has_more_must_agree_with_next_cursor() {
        let claims_more_without_cursor = WindowBounds {
            has_more: true,
            next_cursor: None,
            binding: "b".into(),
        };
        assert!(check_bounds_integrity(&[claims_more_without_cursor], "b").is_err());

        let cursor_without_more = WindowBounds {
            has_more: false,
            next_cursor: Some("c1.abc".into()),
            binding: "b".into(),
        };
        assert!(check_bounds_integrity(&[cursor_without_more], "b").is_err());
    }

    #[test]
    fn a_well_formed_page_passes() {
        let bounds = WindowBounds {
            has_more: true,
            next_cursor: Some("c1.abc".into()),
            binding: "b".into(),
        };
        check_bounds_integrity(&[bounds], "b").unwrap();
    }

    // ── Fixtures ──────────────────────────────────────────────────────────

    const CHANNEL: &str = "11111111-1111-1111-1111-111111111111";

    fn id(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn message(n: u8, created_at: u64) -> serde_json::Value {
        serde_json::json!({
            "id": id(n),
            "kind": 9,
            "pubkey": "aa".repeat(32),
            "created_at": created_at,
            "content": format!("message {n}"),
            "tags": [["h", CHANNEL]],
        })
    }

    fn reply(n: u8, root: &str, created_at: u64, pubkey: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id(n),
            "kind": 9,
            "pubkey": pubkey,
            "created_at": created_at,
            "content": format!("reply {n}"),
            "tags": [["h", CHANNEL], ["e", root, "", "root"]],
        })
    }

    fn bounds_event(binding: &str, has_more: bool, next: Option<(u64, &str)>) -> serde_json::Value {
        let content = serde_json::json!({
            "has_more": has_more,
            "next_cursor": next.map(|(created_at, id)| serde_json::json!({
                "created_at": created_at,
                "id": id,
            })),
        });
        serde_json::json!({
            "id": id(200),
            "kind": KIND_WINDOW_BOUNDS,
            "created_at": 1_700_000_000,
            "content": content.to_string(),
            "tags": [["d", binding]],
        })
    }

    fn summary_event(root: &str, reply_count: u32) -> serde_json::Value {
        let content = serde_json::json!({
            "reply_count": reply_count,
            "descendant_count": reply_count + 1,
            "last_reply_at": 1_700_000_500,
            "participants": ["aa".repeat(32), "bb".repeat(32)],
        });
        serde_json::json!({
            "id": id(201),
            "kind": KIND_THREAD_SUMMARY,
            "created_at": 1_700_000_000,
            "content": content.to_string(),
            "tags": [["e", root]],
        })
    }

    // ── [D-10] parse and bounds integrity ─────────────────────────────────

    /// The binding is verbatim from `expectedBoundsKey`, including the
    /// lowercasing — an id echoed in a different case would discard a page
    /// that was in fact correct.
    #[test]
    fn the_bounds_binding_matches_the_desktop_format() {
        assert_eq!(
            expected_bounds_binding("ABC-DEF", None),
            "abc-def:head",
            "the first page binds to `head`"
        );
        let cursor = Cursor {
            until: 1_700_000_000,
            before_id: "AB".repeat(32),
        };
        assert_eq!(
            expected_bounds_binding("ABC-DEF", Some(&cursor)),
            format!("abc-def:1700000000:{}", "ab".repeat(32))
        );
    }

    #[test]
    fn a_well_formed_window_parses_rows_summaries_and_aux() {
        let root = id(1);
        let events = vec![
            message(1, 1_700_000_100),
            message(2, 1_700_000_050),
            summary_event(&root, 4),
            serde_json::json!({"id": id(50), "kind": 7, "created_at": 1, "tags": [["e", root]]}),
            bounds_event(
                &expected_bounds_binding(CHANNEL, None),
                true,
                Some((1_700_000_050, &id(2))),
            ),
        ];

        let page = parse_window_response(&events, CHANNEL, None).unwrap();
        assert_eq!(page.mode, WindowMode::Nipcw);
        assert_eq!(page.rows.len(), 2, "only content kinds become rows");
        assert_eq!(page.rows[0].thread.as_ref().unwrap().reply_count, 4);
        assert!(page.rows[1].thread.is_none());
        assert_eq!(page.aux.len(), 1, "the reaction is aux, not a row");
        assert!(page.has_more);
        assert_eq!(page.next_cursor.as_ref().unwrap().before_id, id(2));
    }

    /// [D-10]: "overlays are metadata and never render as rows or feed cursor
    /// math." A `39005` or `39006` counted as a row both fabricates a timeline
    /// entry and corrupts the cursor.
    #[test]
    fn overlays_never_become_rows() {
        let events = vec![
            message(1, 1_700_000_100),
            summary_event(&id(1), 2),
            bounds_event(&expected_bounds_binding(CHANNEL, None), false, None),
        ];
        let page = parse_window_response(&events, CHANNEL, None).unwrap();
        assert_eq!(page.rows.len(), 1);
        for row in &page.rows {
            let kind = row.event["kind"].as_u64().unwrap() as u32;
            assert_ne!(kind, KIND_THREAD_SUMMARY);
            assert_ne!(kind, KIND_WINDOW_BOUNDS);
        }
    }

    /// §5.2: "a **full** page with `has_more: false` terminates; row count is
    /// never used as an exhaustion signal." This is the exact-multiple final
    /// page NIP-CW warns about.
    #[test]
    fn a_full_page_with_has_more_false_terminates_at_the_wire_level() {
        let mut events: Vec<serde_json::Value> = (1..=50u8)
            .map(|n| message(n, 1_700_000_000 + u64::from(n)))
            .collect();
        events.push(bounds_event(
            &expected_bounds_binding(CHANNEL, None),
            false,
            None,
        ));
        let page = parse_window_response(&events, CHANNEL, None).unwrap();
        assert_eq!(page.rows.len(), 50, "a full page");
        assert!(!page.has_more, "and it is nonetheless the last one");
        assert!(page.next_cursor.is_none());
    }

    /// A page bound to *someone else's* cursor is discarded, not applied. This
    /// is the check that stops a concurrent request's page from being spliced
    /// into the wrong position in the timeline.
    #[test]
    fn a_page_bound_to_another_cursor_is_discarded() {
        let events = vec![
            message(1, 1_700_000_100),
            bounds_event("someone-elses-page", false, None),
        ];
        let err = parse_window_response(&events, CHANNEL, None).unwrap_err();
        assert_eq!(err.code(), "window_integrity");
    }

    #[test]
    fn a_page_with_no_bounds_overlay_is_discarded() {
        let events = vec![message(1, 1_700_000_100)];
        assert_eq!(
            parse_window_response(&events, CHANNEL, None)
                .unwrap_err()
                .code(),
            "window_integrity"
        );
    }

    #[test]
    fn unparseable_bounds_content_is_discarded_not_defaulted() {
        let events = vec![serde_json::json!({
            "id": id(200),
            "kind": KIND_WINDOW_BOUNDS,
            "content": "{not json",
            "tags": [["d", expected_bounds_binding(CHANNEL, None)]],
        })];
        assert_eq!(
            parse_window_response(&events, CHANNEL, None)
                .unwrap_err()
                .code(),
            "window_integrity"
        );
    }

    /// A malformed `39005` costs one badge refresh; it must not discard a
    /// timeline the operator can otherwise read.
    #[test]
    fn a_malformed_thread_summary_drops_the_badge_not_the_page() {
        let events = vec![
            message(1, 1_700_000_100),
            serde_json::json!({
                "id": id(201),
                "kind": KIND_THREAD_SUMMARY,
                "content": "{not json",
                "tags": [["e", id(1)]],
            }),
            bounds_event(&expected_bounds_binding(CHANNEL, None), false, None),
        ];
        let page = parse_window_response(&events, CHANNEL, None).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert!(page.rows[0].thread.is_none());
    }

    /// The cursor that leaves the daemon is always the [D-6] `c1.` form, so a
    /// client never sees two cursor shapes and never has cause to parse one.
    #[test]
    fn the_next_cursor_round_trips_through_the_versioned_form() {
        let events = vec![
            message(1, 1_700_000_100),
            bounds_event(
                &expected_bounds_binding(CHANNEL, None),
                true,
                Some((1_700_000_100, &id(1))),
            ),
        ];
        let page = parse_window_response(&events, CHANNEL, None).unwrap();
        let cursor = page.next_cursor.unwrap();
        assert!(cursor.encode().starts_with(crate::cursor::CURSOR_PREFIX));
        // And the *next* request binds to it.
        let next_binding = expected_bounds_binding(CHANNEL, Some(&cursor));
        assert!(next_binding.contains(&id(1)));
    }

    // ── [D-10] the degradation branch ─────────────────────────────────────

    /// The downgrade partitions top-level rows from replies itself, because the
    /// standard filter cannot say `top_level: true`.
    #[test]
    fn the_downgrade_partitions_top_level_rows_from_replies() {
        let root = id(1);
        let events = vec![
            message(1, 1_700_000_100),
            reply(2, &root, 1_700_000_110, &"bb".repeat(32)),
            reply(3, &root, 1_700_000_120, &"cc".repeat(32)),
            message(4, 1_700_000_130),
        ];
        let page = assemble_downgraded_page(&events, 50);
        assert_eq!(page.mode, WindowMode::Downgraded);
        assert_eq!(page.rows.len(), 2, "two roots, two rows");
        let thread = page.rows[0].thread.as_ref().unwrap();
        assert_eq!(thread.reply_count, 2, "threads assembled client-side");
        assert_eq!(thread.participants.len(), 2);
        assert_eq!(thread.last_reply_at, Some(1_700_000_120));
        assert!(page.rows[1].thread.is_none(), "a root with no replies");
    }

    /// §5.2: "a no-`39006` response triggers the *downgrade branch*, not a
    /// guess." The two modes are distinguishable on the page, so the decision
    /// is observable rather than inferred.
    #[test]
    fn the_downgrade_is_a_labelled_decision_not_a_silent_fallback() {
        let nipcw = parse_window_response(
            &[bounds_event(
                &expected_bounds_binding(CHANNEL, None),
                false,
                None,
            )],
            CHANNEL,
            None,
        )
        .unwrap();
        let downgraded = assemble_downgraded_page(&[], 50);
        assert_eq!(nipcw.mode, WindowMode::Nipcw);
        assert_eq!(downgraded.mode, WindowMode::Downgraded);
        assert_ne!(nipcw.mode, downgraded.mode);
    }

    /// In the downgraded branch a short page really *is* the end — there is no
    /// server-assembled window for the row count to be wrong about. The
    /// [`WindowMode`] label is what keeps this from being confused with the
    /// inference NIP-CW forbids.
    #[test]
    fn a_short_downgraded_page_terminates_and_a_full_one_continues() {
        let short: Vec<serde_json::Value> = (1..=3u8)
            .map(|n| message(n, 1_700_000_000 + u64::from(n)))
            .collect();
        let page = assemble_downgraded_page(&short, 50);
        assert!(!page.has_more);
        assert!(page.next_cursor.is_none());

        let full: Vec<serde_json::Value> = (1..=50u8)
            .map(|n| message(n, 1_700_000_000 + u64::from(n)))
            .collect();
        let page = assemble_downgraded_page(&full, 50);
        assert!(page.has_more);
        let cursor = page.next_cursor.unwrap();
        assert_eq!(cursor.before_id, id(50), "the cursor is the last raw event");
        assert_eq!(cursor.until, 1_700_000_050);
    }

    /// A full page whose last event has no usable id has nowhere to continue
    /// from. Claiming `has_more` would loop forever on the same page.
    #[test]
    fn a_full_downgraded_page_with_no_usable_cursor_terminates() {
        let mut events: Vec<serde_json::Value> = (1..=2u8)
            .map(|n| message(n, 1_700_000_000 + u64::from(n)))
            .collect();
        events.push(serde_json::json!({"kind": 9, "tags": []}));
        let page = assemble_downgraded_page(&events, 3);
        assert!(
            !page.has_more,
            "has_more must agree with the cursor in the downgrade too"
        );
    }

    /// Aux is aux in both branches — never a row, never cursor input.
    #[test]
    fn the_downgrade_keeps_aux_out_of_the_rows() {
        let events = vec![
            message(1, 1_700_000_100),
            serde_json::json!({"id": id(50), "kind": 7, "created_at": 1, "tags": [["e", id(1)]]}),
            serde_json::json!({"id": id(51), "kind": 5, "created_at": 2, "tags": [["e", id(1)]]}),
        ];
        let page = assemble_downgraded_page(&events, 50);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.aux.len(), 2);
    }

    // ── Kind partitions ───────────────────────────────────────────────────

    /// §5.2 `timeline kind partition`: content vs aux vs non-conversational,
    /// with no phantom unreads from 40099 / 43xxx / 48100.
    #[test]
    fn kind_partitions_are_disjoint_and_complete() {
        for kind in TIMELINE_KINDS {
            assert!(is_content_kind(kind), "{kind} should be content");
            assert!(!is_aux_kind(kind), "{kind} is both content and aux");
        }
        for kind in AUX_KINDS {
            assert!(!is_content_kind(kind), "{kind} is both aux and content");
        }
        for kind in [40099u32, 43001, 43002, 43003, 43004, 43005, 43006, 48100] {
            assert!(is_content_kind(kind), "{kind} renders as a row");
            assert!(
                !is_conversational_unread_kind(kind),
                "{kind} must not create a phantom unread"
            );
        }
        for kind in CONVERSATIONAL_KINDS {
            assert!(is_conversational_unread_kind(kind));
            assert!(is_content_kind(kind));
        }
    }

    /// The unread gate is a whitelist, so an unknown kind fails toward a quiet
    /// badge rather than a phantom one.
    #[test]
    fn an_unknown_kind_creates_no_unread() {
        assert!(!is_conversational_unread_kind(99_999));
    }

    #[test]
    fn aux_kinds_match_the_desktop_verbatim() {
        assert_eq!(AUX_KINDS, [5, 7, 9005, 40003]);
    }

    // ── Thread resolution ─────────────────────────────────────────────────

    #[test]
    fn the_thread_filter_is_e_scoped_with_explicit_kinds() {
        let filter = build_thread_filter(&id(1), 200);
        crate::search::assert_explicit_kinds(&filter, "thread").unwrap();
        assert_eq!(filter["#e"], serde_json::json!([id(1)]));
        assert!(
            filter.get("#h").is_none(),
            "a thread is defined by its root, not by a second channel constraint"
        );
    }

    #[test]
    fn the_aux_backfill_filter_carries_explicit_kinds() {
        let filter = build_aux_backfill_filter(&[id(1), id(2)]);
        crate::search::assert_explicit_kinds(&filter, "aux backfill").unwrap();
        assert_eq!(filter["kinds"], serde_json::json!(AUX_KINDS));
    }

    /// A marked NIP-10 root wins over the first `e` tag; an unmarked reply
    /// falls back to the first one; a message with no `e` tag is top-level.
    #[test]
    fn reply_roots_follow_nip10_markers() {
        let marked = serde_json::json!({
            "tags": [["e", id(9)], ["e", id(1), "", "root"]],
        });
        assert_eq!(reply_root(&marked), Some(id(1)));

        let unmarked = serde_json::json!({"tags": [["e", id(9)]]});
        assert_eq!(reply_root(&unmarked), Some(id(9)));

        assert_eq!(reply_root(&message(1, 1)), None);
    }

    /// The id tiebreak makes the order **total**: two replies in the same
    /// second must not sort differently between two clients.
    #[test]
    fn thread_order_is_total_even_within_one_second() {
        let a = reply(3, &id(1), 1_700_000_100, &"aa".repeat(32));
        let b = reply(2, &id(1), 1_700_000_100, &"bb".repeat(32));
        let forward = sort_thread(vec![a.clone(), b.clone()]);
        let backward = sort_thread(vec![b, a]);
        assert_eq!(forward, backward, "the sort must not depend on input order");
        assert_eq!(forward[0]["id"], serde_json::json!(id(2)));
    }

    #[test]
    fn thread_order_is_chronological() {
        let sorted = sort_thread(vec![
            reply(3, &id(1), 1_700_000_300, &"aa".repeat(32)),
            reply(2, &id(1), 1_700_000_100, &"aa".repeat(32)),
        ]);
        let times: Vec<u64> = sorted
            .iter()
            .map(|e| e["created_at"].as_u64().unwrap())
            .collect();
        assert_eq!(times, [1_700_000_100, 1_700_000_300]);
    }
}
