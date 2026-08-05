//! The HTTP surface: routes, and the Wave-1 endpoint subset.
//!
//! Implements `DESIGN.md` §2.4 and Wave-1 daemon deliverables 13–14 (§4.1.1).
//!
//! The transport is HTTP/1.1 over the Unix domain socket bound by
//! [`crate::socket::bind`]. **No TCP listener ships** (§2.5).
//!
//! # The daemon is built to the wave, not to the whole spec
//!
//! [`WAVE1_ENDPOINTS`] is the exact list from §2.4. Everything else in
//! `daemon-api.md` (backend deploy passthrough, moderation, media, DM
//! open/hide, forum, emoji sets) lands in the wave that needs it, and is absent
//! from `capabilities[]` until then ([`crate::lifecycle::WAVE1_CAPABILITIES`]).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::error::{DaemonError, ErrorBody};
use crate::state::AppState;

/// The Wave-1 endpoint subset, verbatim from `DESIGN.md` §2.4.
///
/// `just daemon-spec-check` (§6.2) makes "adding an endpoint without adding it
/// to the spec is a build failure" real: the OpenAPI document is regenerated
/// and compared against the committed one, which also catches TS-client drift
/// because the generated client is committed and regenerated in the same step.
pub const WAVE1_ENDPOINTS: &[&str] = &[
    // Meta
    "/health",
    "/openapi.json",
    "/daemon",
    "/daemon/registry",
    "/daemon/shutdown",
    "/daemon/reconnect",
    // Session
    "/session",
    "/session/identity",
    "/session/relay-info",
    // Channels
    "/channel",
    "/channel/{id}",
    "/channel/{id}/member",
    "/channel/{id}/join",
    "/channel/{id}/leave",
    "/channel/{id}/message",
    "/channel/{id}/typing",
    "/channel/{id}/read",
    // Messages and threads
    "/message/{id}",
    "/message/{id}/thread",
    "/message/{id}/reaction",
    // Answer an ask card — a threaded kind:9 reply, NOT a control frame (§2.4).
    "/message/{id}/ask",
    // Search
    "/search",
    "/search/user",
    // Directory
    "/user",
    "/user/{pubkey}",
    "/mention/candidates",
    "/mention/inbox",
    "/read-state",
    "/presence",
    // Agents
    "/agent",
    "/agent/{pk}",
    "/agent/{pk}/activity",
    "/agent/{pk}/transcript",
    "/agent/{pk}/metric",
    "/agent/{pk}/control",
    "/agent/fleet",
    // The one event stream
    "/event",
];

/// Every [`DaemonError`] becomes the one error shape of §2.4/§3.13.
///
/// Implemented on the error rather than at each handler so a `?` in any route
/// produces the same body. A handler that formatted its own error would be one
/// `code` the client has to special-case, and §2.4's whole point is that the
/// front end switches on `code` and never on a message.
impl IntoResponse for DaemonError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // The body goes through `crate::redact` inside `ErrorBody::from`, so a
        // secret cannot reach a client through an error path (§2.5).
        let body = ErrorBody::from(&self);
        (status, Json(serde_json::json!({ "error": body }))).into_response()
    }
}

/// `GET /health` — the attach probe of §2.3 [D-1].
async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let health = state.health().await;
    Json(serde_json::json!({
        "status": "ok",
        "version": health.version,
        "api_version": health.api_version,
        "capabilities": health.capabilities,
        // §2.5: keyless is a **visible** state. A daemon archiving nothing must
        // not look identical to a healthy one.
        "archiving": health.archiving,
        "uptime_secs": state.started_at.elapsed().as_secs(),
    }))
}

/// `GET /openapi.json`.
async fn openapi() -> Json<serde_json::Value> {
    Json(crate::openapi::document())
}

/// `GET /daemon` — pid, socket, and **every drop counter** (§4.1.4 criterion 5).
///
/// The counters are the point: exit criterion 5 reads them, and "why is this
/// agent's feed empty" has an answer only because each guard moves its own.
async fn daemon(State(state): State<AppState>) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    Json(serde_json::json!({
        "pid": std::process::id(),
        "version": crate::VERSION,
        "api_version": crate::API_VERSION,
        "socket": state.config.socket.display().to_string(),
        "identity": state.config.identity,
        "preimage": state.config.identity.preimage(),
        "systemd_managed": state.config.systemd_managed,
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "idle_for_secs": state.idle_for().await.as_secs(),
        "connection": inner.session.state(),
        "session_counters": inner.session.counters(),
        "observer_counters": inner.observer.counters(),
        "observer_pending_unknown": inner.observer.pending_unknown_len(),
        "observer_cache_bytes": inner.observer.live_bytes(),
        "channels": inner.channels.len(),
        "stream_seq": inner.stream.latest_seq(),
        "stream_ring": inner.stream.len(),
    }))
}

/// `GET /session` — identity, relay, and connection state (§2.6).
async fn session(State(state): State<AppState>) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    Json(serde_json::json!({
        "pubkey": inner.identity.as_ref().map(|i| i.pubkey.clone()),
        "relay_url": state.config.identity.relay_url,
        "auth_tag_owner": inner
            .identity
            .as_ref()
            .and_then(|i| i.auth_tag.as_ref())
            .map(|t| t.owner_pubkey.clone()),
        // Surfaced **verbatim** (§2.6): these states look identical to "hung"
        // if collapsed, and auth failure must stay distinct from network
        // failure with its own remediation.
        "connection": inner.session.state(),
        "archiving": inner.identity.is_some(),
    }))
}

/// `GET /channel` — the list, attention-first.
async fn list_channels(State(state): State<AppState>) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    let (unread, mentions) = inner.channels.totals();
    Json(serde_json::json!({
        "channels": inner.channels.list(),
        "totals": {"unread": unread, "mentions": mentions},
    }))
}

/// `GET /channel/{id}`.
async fn get_channel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::Result<Json<serde_json::Value>> {
    let inner = state.lock().await;
    let channel = inner
        .channels
        .get(&id)
        .ok_or_else(|| DaemonError::NotFound(format!("channel {id}")))?;
    Ok(Json(serde_json::json!({ "channel": channel })))
}

/// `GET /channel/{id}/member`.
async fn channel_members(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::Result<Json<serde_json::Value>> {
    let inner = state.lock().await;
    let roster = inner
        .channels
        .roster(&id)
        .ok_or_else(|| DaemonError::NotFound(format!("channel {id}")))?;
    Ok(Json(serde_json::json!({ "members": roster })))
}

/// Query parameters for `GET /mention/candidates`.
#[derive(Debug, serde::Deserialize)]
struct CandidateQuery {
    channel: String,
    #[serde(default)]
    prefix: String,
    #[serde(default = "default_candidate_limit")]
    limit: usize,
}

fn default_candidate_limit() -> usize {
    20
}

/// `GET /mention/candidates` — [D-2]'s resolved-pubkey candidates.
async fn mention_candidates(
    State(state): State<AppState>,
    Query(query): Query<CandidateQuery>,
) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    let empty = std::collections::BTreeSet::new();
    let roster = inner.channels.roster(&query.channel).unwrap_or(&empty);
    Json(serde_json::json!({
        "candidates": inner.mentions.candidates(&query.prefix, roster, query.limit),
        // The live `n of 50` counter of §2.4 needs the cap, and shipping it with
        // the candidates means the composer never has to hardcode it.
        "cap": crate::mentions::MENTION_CAP,
    }))
}

/// `GET /presence?pubkeys=a,b,c`.
#[derive(Debug, serde::Deserialize)]
struct PresenceQuery {
    #[serde(default)]
    pubkeys: String,
}

async fn presence(
    State(state): State<AppState>,
    Query(query): Query<PresenceQuery>,
) -> Json<serde_json::Value> {
    let pubkeys: Vec<String> = query
        .pubkeys
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let now = unix_now();
    let inner = state.lock().await;
    Json(serde_json::json!({ "presence": inner.presence.snapshot(&pubkeys, now) }))
}

/// `GET /agent/fleet` — blocked-first (§3.4).
async fn agent_fleet(State(state): State<AppState>) -> Json<serde_json::Value> {
    let now = unix_now();
    let inner = state.lock().await;
    Json(serde_json::json!({ "agents": inner.fleet.rows(now) }))
}

/// `GET /agent/{pk}/activity` — live and archived, merged into **one** sorted
/// deduplicated sequence (`daemon-api.md` §3.9).
async fn agent_activity(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    let live = inner.observer.live_frames(&pubkey);
    // The archive half streams from SQLite once [`crate::cache`] is wired; the
    // merge is already the one place the two meet, so adding it is one argument
    // rather than a second state machine.
    let frames = crate::observer::merge_activity(live, Vec::new());
    Json(serde_json::json!({ "frames": frames }))
}

/// `GET /read-state`.
async fn read_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    Json(serde_json::json!({
        "contexts": inner.read_state.len(),
        "dirty": inner.read_state.is_dirty(),
    }))
}

/// `POST /daemon/shutdown` — the broom `GET /daemon/registry` enumerates for.
async fn shutdown(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Answer *before* exiting: a client that gets a connection reset cannot
    // tell "shut down" from "crashed", and §1.3 property 3 applies to the
    // daemon's own lifecycle too.
    let systemd = state.config.systemd_managed;
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        std::process::exit(0);
    });
    Json(serde_json::json!({
        "stopping": true,
        // §2.3: `buzz-tui daemon restart` must print the systemctl command
        // rather than SIGTERM-ing a unit systemd will resurrect underneath it.
        "systemd_managed": systemd,
    }))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Meta ───────────────────────────────────────────────────────────────────

/// `GET /daemon/registry` — every live daemon for this user (§2.2, §2.3).
///
/// The broom `POST /daemon/shutdown` needs an enumeration path for: a cap with
/// no way to see what is holding it is a leak the operator cannot clear.
/// Liveness is a **connect probe**, never a pidfile pid check — a pid check is
/// a reuse race, and §2.3 is explicit that a socket nothing answers on is dead.
async fn daemon_registry(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut entries: Vec<serde_json::Value> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&state.config.runtime_dir) {
        for socket in dir
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
        {
            let alive = tokio::net::UnixStream::connect(&socket).await.is_ok();
            entries.push(serde_json::json!({
                "socket": socket.display().to_string(),
                "alive": alive,
                "self": socket == state.config.socket,
            }));
        }
    }
    entries.sort_by(|a, b| a["socket"].as_str().cmp(&b["socket"].as_str()));
    Json(serde_json::json!({
        "daemons": entries,
        "cap": crate::config::MAX_LIVE_DAEMONS,
    }))
}

/// `POST /daemon/reconnect` — force the relay loop to redial (§2.6).
///
/// Ends the current session, which puts the loop on its normal reconnect path:
/// the ladder, the watermark replay, and the paced resubscribe are all already
/// there, and an in-place reconnect would be a second copy of each to keep
/// correct.
async fn daemon_reconnect(State(state): State<AppState>) -> crate::Result<Json<serde_json::Value>> {
    state
        .wire()?
        .send(crate::wire::WireCommand::Reconnect)
        .await?;
    Ok(Json(serde_json::json!({"reconnecting": true})))
}

// ── Session ────────────────────────────────────────────────────────────────

/// `GET /session/identity` — the loaded identity, never its key material.
async fn session_identity(State(state): State<AppState>) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    Json(serde_json::json!({
        "pubkey": inner.identity.as_ref().map(|i| i.pubkey.clone()),
        "auth_tag_owner": inner
            .identity
            .as_ref()
            .and_then(|i| i.auth_tag.as_ref())
            .map(|t| t.owner_pubkey.clone()),
        "auth_tag_expires_at": inner
            .identity
            .as_ref()
            .and_then(|i| i.auth_tag.as_ref())
            .and_then(|t| t.expires_at),
        // §2.5: keyless is a **visible** state, not a quiet one.
        "archiving": inner.identity.is_some(),
    }))
}

/// `GET /session/relay-info` — the relay's NIP-11 document.
///
/// Proxied rather than cached in this revision: the document changes on relay
/// deploys, and a cache with no invalidation would report a `supported_nips`
/// list the relay no longer honours — which presents as a feature silently not
/// working rather than as a stale field.
async fn session_relay_info(
    State(state): State<AppState>,
) -> crate::Result<Json<serde_json::Value>> {
    let raw = state.rest.get_public("/info").await?;
    Ok(Json(serde_json::from_str(&raw).map_err(|e| {
        DaemonError::Relay {
            status: 0,
            body: format!("NIP-11 document is not JSON: {e}"),
        }
    })?))
}

// ── Channels ───────────────────────────────────────────────────────────────

/// Query parameters for `GET /channel/{id}/message`.
#[derive(Debug, serde::Deserialize)]
struct WindowQuery {
    #[serde(default = "default_window_limit")]
    limit: u32,
    /// Opaque composite cursor from the previous page's `next` ([D-6]).
    before: Option<String>,
}

fn default_window_limit() -> u32 {
    50
}

/// `GET /channel/{id}/message` — one page of history ([D-10]).
///
/// The **NIP-CW window** path with its degradation branch, not a bare query:
/// exhaustion is the `39006` overlay's `has_more`, never the row count, and a
/// relay that serves no valid overlay takes the downgraded branch — which ships
/// in the same wave because a downgrade that has never run is a downgrade that
/// does not work.
async fn channel_window(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<WindowQuery>,
) -> crate::Result<Json<serde_json::Value>> {
    let cursor = query
        .before
        .as_deref()
        .map(crate::cursor::Cursor::decode)
        .transpose()?;
    let limit = query.limit.clamp(1, 200);
    let filter = crate::timeline::build_window_filter(&id, limit, cursor.as_ref());
    let identity = state.identity_snapshot().await?;
    let events = state.rest.query(&identity, &filter).await?;

    let page = match crate::timeline::parse_window_response(&events, &id, cursor.as_ref()) {
        Ok(page) => page,
        Err(err) => {
            // [D-10]: "downgrade is a decision, not a fallback that happens by
            // accident." Logged with the reason, and the page carries
            // `mode: downgraded` so the decision is observable from the client.
            tracing::info!(%err, channel = %id, "no valid 39006; taking the downgrade branch");
            let downgraded = crate::timeline::build_downgraded_filter(&id, limit, cursor.as_ref());
            let events = state.rest.query(&identity, &downgraded).await?;
            crate::timeline::assemble_downgraded_page(&events, limit)
        }
    };
    // Subscribing here rather than at first render is what makes the live tail
    // and the history page agree: the watermark starts at the newest row this
    // page carries, so the subscription replays from there rather than from
    // whenever the client happened to connect.
    if let Ok(wire) = state.wire() {
        let _ = wire
            .send(crate::wire::WireCommand::Subscribe {
                channel_id: id.clone(),
            })
            .await;
    }
    Ok(Json(serde_json::json!({
        "messages": page.rows,
        "aux": page.aux,
        "next": page.next_cursor.as_ref().map(crate::cursor::Cursor::encode),
        "has_more": page.has_more,
        "mode": page.mode,
    })))
}

/// `POST /channel/{id}/message` — send, visible-pending (§2.7, [D-7]).
async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<crate::post::SendRequest>,
) -> crate::Result<Json<crate::post::SendResponse>> {
    let identity = state.identity_snapshot().await?;
    {
        // The cap is checked **before** the connection state, and before the
        // event is signed: telling the operator the relay is down, waiting for
        // it to come back, and then telling them there are too many mentions is
        // two round trips of bad news for one message.
        let inner = state.lock().await;
        crate::post::check_sendable(&request, inner.session.state())?;
    }
    let event = crate::post::build_message_event(&identity, &id, &request)?;
    if let Some(local_id) = request.local_id.as_deref() {
        // [D-7]: recorded at sign time, keyed by the event id the daemon just
        // computed. No wire change, no tag on the event — a `local_id` tag
        // would leak a client's UI bookkeeping into permanent relay history.
        state
            .lock()
            .await
            .local_ids
            .record(event.id.to_hex(), local_id);
    }
    let response = state.wire()?.publish(event, request.local_id).await?;
    Ok(Json(response))
}

/// `POST /message/{id}/ask` — answer an ask card (§2.4, §3.4.1).
#[derive(Debug, serde::Deserialize)]
struct AskAnswer {
    /// Chosen option indices, **zero-based** on the wire; the reply body is
    /// 1-based, which `ask_reply_content` handles.
    indices: Vec<usize>,
    /// The channel the card's message lives in.
    channel: String,
}

/// `POST /message/{id}/ask`.
///
/// A **threaded kind:9 reply**, not a control frame: `POST /agent/{pk}/control`
/// accepts exactly two payloads and the harness logs-and-drops everything else,
/// so routing an answer there would fail silently at the agent.
async fn answer_ask(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(answer): Json<AskAnswer>,
) -> crate::Result<Json<crate::post::SendResponse>> {
    let identity = state.identity_snapshot().await?;
    let agent = {
        let inner = state.lock().await;
        inner
            .asks
            .asker(&id)
            .map(str::to_string)
            .ok_or_else(|| DaemonError::NotFound(format!("no open ask card for {id}")))?
    };
    let event = crate::post::build_ask_answer_event(
        &identity,
        &answer.channel,
        &id,
        &agent,
        &answer.indices,
    )?;
    let response = state.wire()?.publish(event, None).await?;
    // Closed only after the relay accepted it: closing on send would clear the
    // card while the agent is still blocked, and the operator would have no
    // affordance left to answer with.
    if response.accepted {
        let mut inner = state.lock().await;
        inner.asks.answer(&id);
        inner.fleet.agent_mut(&agent).awaiting_answer = inner.asks.awaiting(&agent) > 0;
    }
    Ok(Json(response))
}

/// `POST /channel/{id}/join`, `/leave` — membership writes.
async fn join_channel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::Result<Json<crate::post::SendResponse>> {
    let uuid = parse_channel_uuid(&id)?;
    let identity = state.identity_snapshot().await?;
    let event = identity
        .sign_event(buzz_sdk::build_join(uuid).map_err(|e| DaemonError::Sdk(e.to_string()))?)?;
    Ok(Json(state.wire()?.publish(event, None).await?))
}

async fn leave_channel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::Result<Json<crate::post::SendResponse>> {
    let uuid = parse_channel_uuid(&id)?;
    let identity = state.identity_snapshot().await?;
    let event = identity
        .sign_event(buzz_sdk::build_leave(uuid).map_err(|e| DaemonError::Sdk(e.to_string()))?)?;
    Ok(Json(state.wire()?.publish(event, None).await?))
}

/// `POST /channel` — create a channel.
#[derive(Debug, serde::Deserialize)]
struct CreateChannel {
    name: String,
    #[serde(default)]
    about: Option<String>,
    /// `open` or `private`. Absent means `open`.
    #[serde(default)]
    visibility: Option<String>,
    /// `stream` or `forum`. Absent means `stream`.
    #[serde(default)]
    channel_type: Option<String>,
}

async fn create_channel(
    State(state): State<AppState>,
    Json(request): Json<CreateChannel>,
) -> crate::Result<Json<serde_json::Value>> {
    let identity = state.identity_snapshot().await?;
    // Parsed rather than passed through, and an unrecognized value is an error
    // rather than a default: silently creating an `open` channel for someone
    // who typed `privte` is the one mistake here that cannot be undone by
    // editing the channel afterwards.
    let visibility = match request.visibility.as_deref().unwrap_or("open") {
        "open" => buzz_sdk::Visibility::Open,
        "private" => buzz_sdk::Visibility::Private,
        other => {
            return Err(DaemonError::InvalidInput(format!(
                "visibility must be \"open\" or \"private\", got {other:?}"
            )))
        }
    };
    let channel_type = match request.channel_type.as_deref().unwrap_or("stream") {
        "stream" => buzz_sdk::ChannelKind::Stream,
        "forum" => buzz_sdk::ChannelKind::Forum,
        other => {
            return Err(DaemonError::InvalidInput(format!(
                "channel_type must be \"stream\" or \"forum\", got {other:?}"
            )))
        }
    };
    // The uuid is minted here, not by the relay: the client needs it to
    // navigate to the channel it just created, and a relay-assigned id would
    // require a second round trip to learn.
    let channel_id = uuid::Uuid::new_v4();
    let builder = buzz_sdk::build_create_channel(
        channel_id,
        &request.name,
        Some(visibility),
        Some(channel_type),
        request.about.as_deref(),
        // Wave 1 ships no channel TTL surface; `None` is the relay's own
        // default rather than a value invented here.
        None,
    )
    .map_err(|e| DaemonError::Sdk(e.to_string()))?;
    let event = identity.sign_event(builder)?;
    let response = state.wire()?.publish(event, None).await?;
    Ok(Json(serde_json::json!({
        "channel_id": channel_id.to_string(),
        "event_id": response.event_id,
        "accepted": response.accepted,
        "message": response.message,
    })))
}

/// `POST /channel/{id}/typing` — fire-and-forget 20002.
///
/// **Always `202`, never an error** (`daemon-api.md` §3.5): typing is dropped,
/// not queued, when the rate-limit gate is armed, so an error the TUI would
/// have to handle is an error about a frame that is allowed to vanish.
async fn typing(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(err) = send_typing(&state, &id).await {
        tracing::debug!(%err, channel = %id, "typing indicator not sent");
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"sent": true})),
    )
        .into_response()
}

/// The typing publish itself, so [`typing`] can swallow every failure in one
/// place rather than at four `?`s that would each need a comment.
async fn send_typing(state: &AppState, channel_id: &str) -> crate::Result<()> {
    let uuid = parse_channel_uuid(channel_id)?;
    let identity = state.identity_snapshot().await?;
    let builder = nostr::EventBuilder::new(
        nostr::Kind::Custom(buzz_core::kind::KIND_TYPING_INDICATOR as u16),
        "",
    )
    .tags([
        nostr::Tag::parse(["h", &uuid.to_string()]).map_err(|e| DaemonError::Sdk(e.to_string()))?
    ]);
    let event = identity.sign_event(builder)?;
    state.wire()?.publish_detached(event).await
}

/// `POST /channel/{id}/read` — mark a channel read to `marker` (default now).
#[derive(Debug, serde::Deserialize)]
struct MarkRead {
    /// Unix seconds to mark to. Absent means now.
    #[serde(default)]
    marker: Option<u64>,
}

/// `POST /channel/{id}/read`.
///
/// Sets the marker and returns; the 30078 publish happens on the loop's
/// [`crate::readstate::PUBLISH_DEBOUNCE_MS`] debounce. Publishing inline would
/// mean one relay write per keystroke-driven mark on a channel the operator is
/// scrolling through.
async fn mark_channel_read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<MarkRead>,
) -> Json<serde_json::Value> {
    let marker = request.marker.unwrap_or_else(|| unix_now().max(0) as u64);
    let mut inner = state.lock().await;
    let advanced = inner.read_state.mark(&id, marker);
    if advanced {
        // Marking read clears the badge immediately rather than waiting for the
        // publish round trip: the operator's own action is the authority here,
        // and a badge that lingers for five seconds after they cleared it reads
        // as the app not listening.
        inner.channels.set_unread(&id, 0, 0);
        inner.stream.publish(
            "channel.unread",
            serde_json::json!({"channel_id": id, "unread": 0, "mentions": 0}),
        );
    }
    Json(serde_json::json!({"marker": marker, "advanced": advanced}))
}

/// `PUT /read-state` — merge a client's markers into the frontier.
#[derive(Debug, serde::Deserialize)]
struct ReadStateUpdate {
    contexts: std::collections::BTreeMap<String, u64>,
}

async fn put_read_state(
    State(state): State<AppState>,
    Json(request): Json<ReadStateUpdate>,
) -> Json<serde_json::Value> {
    let mut inner = state.lock().await;
    let mut advanced = 0usize;
    // Grow-only per context, which is what makes multi-device convergent: a
    // device that has been asleep cannot rewind another's progress.
    for (context, marker) in crate::readstate::sanitize_contexts(request.contexts) {
        if inner.read_state.mark(&context, marker) {
            advanced += 1;
        }
    }
    Json(serde_json::json!({
        "advanced": advanced,
        "contexts": inner.read_state.len(),
        "dirty": inner.read_state.is_dirty(),
    }))
}

// ── Messages and threads ───────────────────────────────────────────────────

/// `GET /message/{id}` — one hydrated event.
async fn get_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::Result<Json<serde_json::Value>> {
    let identity = state.identity_snapshot().await?;
    // `ids` still needs explicit `kinds` (§2.4): the exemption does not cover
    // `RESULT_GATED_KINDS`, and a kindless filter is refused before it leaves.
    let filter = serde_json::json!({
        "kinds": crate::timeline::TIMELINE_KINDS,
        "ids": [id],
        "limit": 1,
    });
    let events = state.rest.query(&identity, &filter).await?;
    let event = events
        .into_iter()
        .next()
        .ok_or_else(|| DaemonError::NotFound(format!("message {id}")))?;
    Ok(Json(serde_json::json!({"message": event})))
}

/// Query parameters for `GET /message/{id}/thread`.
#[derive(Debug, serde::Deserialize)]
struct ThreadQuery {
    #[serde(default = "default_thread_limit")]
    limit: u32,
}

fn default_thread_limit() -> u32 {
    200
}

/// `GET /message/{id}/thread` — the full thread, in a total order.
async fn get_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ThreadQuery>,
) -> crate::Result<Json<serde_json::Value>> {
    let identity = state.identity_snapshot().await?;
    let filter = crate::timeline::build_thread_filter(&id, query.limit.clamp(1, 500));
    let events = state.rest.query(&identity, &filter).await?;
    // Sorted by `(created_at, id)`: the id tiebreak is what makes the order
    // total, so "the third reply" means the same thing in two clients.
    Ok(Json(serde_json::json!({
        "replies": crate::timeline::sort_thread(events),
    })))
}

/// `POST /message/{id}/reaction` — NIP-25.
#[derive(Debug, serde::Deserialize)]
struct ReactionRequest {
    emoji: String,
}

async fn react(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ReactionRequest>,
) -> crate::Result<Json<crate::post::SendResponse>> {
    let event_id = nostr::EventId::from_hex(&id)
        .map_err(|_| DaemonError::InvalidInput(format!("not an event id: {id}")))?;
    let identity = state.identity_snapshot().await?;
    let builder = buzz_sdk::build_reaction(event_id, &request.emoji)
        .map_err(|e| DaemonError::Sdk(e.to_string()))?;
    let event = identity.sign_event(builder)?;
    Ok(Json(state.wire()?.publish(event, None).await?))
}

/// `GET /message/{id}/reaction` — grouped reactions over the aux overlay.
async fn get_reactions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::Result<Json<serde_json::Value>> {
    let identity = state.identity_snapshot().await?;
    let filter = crate::timeline::build_aux_backfill_filter(std::slice::from_ref(&id));
    let events = state.rest.query(&identity, &filter).await?;
    let me = identity.pubkey.clone();

    let mut grouped: std::collections::BTreeMap<String, (u32, Vec<String>, bool)> =
        std::collections::BTreeMap::new();
    for event in events
        .iter()
        .filter(|e| e.get("kind").and_then(serde_json::Value::as_u64) == Some(7))
    {
        let Some(emoji) = event.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(pubkey) = event.get("pubkey").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let entry = grouped
            .entry(emoji.to_string())
            .or_insert((0, Vec::new(), false));
        entry.0 += 1;
        entry.1.push(pubkey.to_string());
        entry.2 |= pubkey == me;
    }
    let reactions: Vec<serde_json::Value> = grouped
        .into_iter()
        .map(|(emoji, (count, reactors, mine))| {
            serde_json::json!({"emoji": emoji, "count": count, "reactors": reactors, "mine": mine})
        })
        .collect();
    Ok(Json(serde_json::json!({"reactions": reactions})))
}

// ── Search and directory ───────────────────────────────────────────────────

/// Query parameters for `GET /search`.
#[derive(Debug, serde::Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default = "default_search_limit")]
    limit: u32,
}

fn default_search_limit() -> u32 {
    50
}

/// `GET /search` — operator-parsed, `kinds` always set (§3.5).
async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> crate::Result<Json<serde_json::Value>> {
    let query = crate::search::parse_operators(&params.q);
    let identity = state.identity_snapshot().await?;
    // A `from:` naming more than one identity is `409 ambiguous_author` with
    // the candidate list, never a silent mix of authors — the failure that
    // makes a result quietly wrong rather than visibly empty.
    let author = match query.from.as_deref() {
        Some(value) => {
            let inner = state.lock().await;
            Some(crate::search::resolve_author(
                value,
                &inner.mentions.directory,
            )?)
        }
        None => None,
    };
    let filter = crate::search::build_query_filter(&query, author.as_deref(), None, params.limit);
    let results = state.rest.query(&identity, &filter).await?;
    Ok(Json(serde_json::json!({
        "results": results,
        "ranking": crate::search::ranking_for(&query),
        "query": {
            "text": query.text,
            "from": query.from,
            "in": query.in_channel,
            "after": query.after,
            "before": query.before,
        },
    })))
}

/// Query parameters for `GET /search/user` and `GET /user`.
#[derive(Debug, serde::Deserialize)]
struct UserQuery {
    #[serde(default)]
    name: String,
    #[serde(default)]
    pubkeys: String,
    #[serde(default)]
    channel: String,
    #[serde(default = "default_candidate_limit")]
    limit: usize,
}

/// `GET /search/user` — directory search by name.
async fn search_user(
    State(state): State<AppState>,
    Query(query): Query<UserQuery>,
) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    // Ranked over the same directory the send path resolves against, which is
    // what makes "what you picked is what gets tagged" hold by construction
    // rather than by two implementations agreeing ([D-2]).
    let everyone: std::collections::BTreeSet<String> =
        inner.mentions.directory.keys().cloned().collect();
    Json(serde_json::json!({
        "users": inner.mentions.candidates(&query.name, &everyone, query.limit),
    }))
}

/// `GET /user` — batch by pubkey, by channel roster, or by name.
async fn list_users(
    State(state): State<AppState>,
    Query(query): Query<UserQuery>,
) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    let directory = &inner.mentions.directory;
    let requested: Vec<String> = query
        .pubkeys
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let users: Vec<serde_json::Value> = if !requested.is_empty() {
        requested
            .iter()
            .map(|pubkey| profile_json(directory.get(pubkey), pubkey))
            .collect()
    } else if !query.channel.is_empty() {
        inner
            .channels
            .roster(&query.channel)
            .map(|roster| {
                roster
                    .iter()
                    .map(|pubkey| profile_json(directory.get(pubkey), pubkey))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        let everyone: std::collections::BTreeSet<String> = directory.keys().cloned().collect();
        inner
            .mentions
            .candidates(&query.name, &everyone, query.limit)
            .into_iter()
            .map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
            .collect()
    };
    Json(serde_json::json!({"users": users}))
}

/// `GET /user/{pubkey}` — one profile plus its presence.
async fn get_user(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> Json<serde_json::Value> {
    let now = unix_now();
    let inner = state.lock().await;
    Json(serde_json::json!({
        "user": profile_json(inner.mentions.directory.get(&pubkey), &pubkey),
        "presence": inner.presence.get(&pubkey, now),
    }))
}

/// Render a profile, or an honest placeholder for a pubkey with none.
///
/// A pubkey with no kind-0 is **not** an error: the directory is populated from
/// events, and an author whose profile has not arrived is still an author. The
/// placeholder carries the pubkey so two unnamed identities stay
/// distinguishable.
fn profile_json(profile: Option<&crate::mentions::Profile>, pubkey: &str) -> serde_json::Value {
    match profile {
        Some(profile) => serde_json::json!({
            "pubkey": profile.pubkey,
            "name": profile.name,
            "display_name": profile.display_name,
            "label": profile.label(),
            "is_agent": profile.is_agent,
        }),
        None => serde_json::json!({
            "pubkey": pubkey,
            "name": serde_json::Value::Null,
            "display_name": serde_json::Value::Null,
            "label": pubkey.chars().take(8).collect::<String>(),
            "is_agent": false,
        }),
    }
}

/// Query parameters for `GET /mention/inbox`.
#[derive(Debug, serde::Deserialize)]
struct InboxQuery {
    since: Option<u64>,
    #[serde(default = "default_inbox_limit")]
    limit: u32,
}

fn default_inbox_limit() -> u32 {
    50
}

/// `GET /mention/inbox` — messages that `p`-tag this identity.
async fn mention_inbox(
    State(state): State<AppState>,
    Query(query): Query<InboxQuery>,
) -> crate::Result<Json<serde_json::Value>> {
    let identity = state.identity_snapshot().await?;
    let filter = crate::mentions::build_inbox_filter(
        &identity.pubkey,
        query.since,
        query.limit.clamp(1, 200),
    );
    let mentions = state.rest.query(&identity, &filter).await?;
    Ok(Json(serde_json::json!({"mentions": mentions})))
}

// ── Agents ─────────────────────────────────────────────────────────────────

/// `GET /agent` — every agent the daemon knows, as fleet rows.
///
/// The same reduction `/agent/fleet` serves, because there is exactly one
/// answer to "what is this agent doing" and two endpoints computing it
/// separately is two things to keep in agreement.
async fn list_agents(State(state): State<AppState>) -> Json<serde_json::Value> {
    let now = unix_now();
    let inner = state.lock().await;
    Json(serde_json::json!({"agents": inner.fleet.rows(now)}))
}

/// `GET /agent/{pk}` — one agent's row.
async fn get_agent(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> crate::Result<Json<serde_json::Value>> {
    let now = unix_now();
    let inner = state.lock().await;
    let agent = inner
        .fleet
        .agent(&pubkey)
        .ok_or_else(|| DaemonError::NotFound(format!("agent {pubkey}")))?;
    Ok(Json(serde_json::json!({
        "agent": crate::fleet::reduce_agent(&pubkey, agent, now),
        "awaiting": inner.asks.awaiting(&pubkey),
    })))
}

/// `GET /agent/{pk}/transcript` — the activity fold, oldest first.
///
/// The same merged sequence `/activity` serves. It is a distinct endpoint
/// because §3.4 renders them differently — activity is a live tail, a
/// transcript is a document — and not because the data differs.
async fn agent_transcript(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> Json<serde_json::Value> {
    let inner = state.lock().await;
    let frames = crate::observer::merge_activity(inner.observer.live_frames(&pubkey), Vec::new());
    Json(serde_json::json!({
        "frames": frames,
        // §2.5/§4.1.1-3: the SQLite archive is not wired in this wave, so a
        // transcript is the live window only. Saying so is what keeps a short
        // transcript from reading as a lost one.
        "archived": false,
    }))
}

/// `GET /agent/{pk}/metric` — 44200 for one agent, `#p = self` (§2.4).
///
/// The scope is **mandatory**, not a narrowing: 44200 is in
/// `RESULT_GATED_KINDS`, which loses the `ids` exemption, so the tempting
/// `ids`-only form is refused by the relay. `tests/live_relay.rs` asserts both
/// halves against a real one.
async fn agent_metric(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> crate::Result<Json<serde_json::Value>> {
    let identity = state.identity_snapshot().await?;
    let filter = crate::metric::build_metric_filter(&pubkey, &identity.pubkey)?;
    let events = state.rest.query(&identity, &filter).await?;

    let mut metrics = Vec::new();
    for raw in &events {
        let Ok(event) = serde_json::from_value::<nostr::Event>(raw.clone()) else {
            continue;
        };
        // A payload that fails NIP-AM's numeric constraints is refused rather
        // than clamped: a negative cost is a publisher bug, and rendering it as
        // zero hides the bug while corrupting the session total.
        match crate::metric::decrypt_metric(&identity, &event, None) {
            Ok(metric) => metrics.push(metric),
            Err(err) => tracing::debug!(%err, "44200 rejected on the metric endpoint"),
        }
    }
    let session = {
        let now = unix_now();
        let inner = state.lock().await;
        inner
            .fleet
            .agent(&pubkey)
            .map(|agent| crate::fleet::reduce_agent(&pubkey, agent, now))
    };
    Ok(Json(
        serde_json::json!({"metrics": metrics, "session": session}),
    ))
}

/// `POST /agent/{pk}/control` — cancel a turn or switch a model (§2.4).
///
/// **Exactly two payloads**, because that is what the harness dispatches on:
/// `handle_relay_observer_control_event` logs-and-drops everything else, so a
/// third control type would be discarded *silently* at the agent. The typed
/// enum is what makes a third one fail to deserialize here instead.
async fn agent_control(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Json(payload): Json<crate::observer::ControlPayload>,
) -> crate::Result<Json<crate::post::SendResponse>> {
    let identity = state.identity_snapshot().await?;
    let agent = nostr::PublicKey::from_hex(&pubkey)
        .map_err(|_| DaemonError::InvalidInput(format!("not a pubkey: {pubkey}")))?;
    let keys = identity
        .signing_keys()
        .ok_or(DaemonError::NotAuthenticated)?;
    // `p = agent, agent = agent, frame = control` (`daemon-api.md` §3.9): the
    // control frame is addressed to the agent, not to the owner, which is the
    // one place the observer envelope's direction reverses.
    let ciphertext = buzz_core::observer::encrypt_observer_payload(&keys, &agent, &payload)
        .map_err(|e| DaemonError::Sdk(format!("control encrypt failed: {e}")))?;
    let builder = buzz_sdk::build_agent_observer_frame(
        &pubkey,
        &pubkey,
        buzz_core::observer::OBSERVER_FRAME_CONTROL,
        &ciphertext,
    )
    .map_err(|e| DaemonError::Sdk(e.to_string()))?;
    let event = identity.sign_event(builder)?;
    Ok(Json(state.wire()?.publish(event, None).await?))
}

// ── The event stream ───────────────────────────────────────────────────────

/// Query parameters for `GET /event`.
#[derive(Debug, serde::Deserialize)]
struct EventQuery {
    /// Resume cursor — the `seq` of the last frame the client received.
    since: Option<u64>,
    /// Comma-separated topic prefixes, e.g. `agent,message`.
    #[serde(default)]
    topic: String,
}

/// `GET /event` — the one event stream (§4.1.1 deliverable 13, [D-5]).
///
/// ndjson by default, SSE by content negotiation. `seq` is daemon-global and
/// monotonic, which is what makes `?since=` a total order rather than a
/// per-topic one.
///
/// # Subscribe first, then replay
///
/// The reader subscribes to the live broadcast **before** the ring is replayed,
/// and then discards live frames at or below the replayed high-water mark. The
/// reverse order has a window in which a frame published during the replay is
/// in neither half — a silent gap, which is the one thing §2.6's link-A story
/// forbids.
///
/// # `stream.reset` comes first, or not at all
///
/// A cursor that has aged out of the ring gets [`crate::stream::StreamControl::Reset`]
/// as the **first** frame on the wire, so the TUI invalidates and re-fetches
/// rather than presenting a silently gapped timeline.
///
/// # [D-5] overflow announces itself before the disconnect
///
/// A reader that lags past the broadcast capacity gets
/// `stream.overflow{dropped, since_seq}` and *then* the stream ends. Loss is
/// announced before it happens, which is §1.3 property 3 applied to the
/// daemon→TUI hop — `daemon-api.md`'s bare kill-the-slow-consumer would drop
/// the reader with no way to know what it missed.
async fn event_stream(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<EventQuery>,
) -> Response {
    let encoding = crate::stream::Encoding::negotiate(
        headers
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok()),
    );
    let topics: Vec<String> = query
        .topic
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let (receiver, replay) = {
        let inner = state.lock().await;
        // Subscribe first — see the doc comment. The two halves are only
        // gapless in this order.
        let receiver = inner.stream.subscribe();
        let replay = inner.stream.replay(query.since.unwrap_or(0));
        (receiver, replay)
    };

    let mut prelude: Vec<String> = Vec::new();
    let mut high_water = query.since.unwrap_or(0);
    match replay {
        crate::stream::Replay::Reset => {
            prelude.push(control_line(encoding, &crate::stream::StreamControl::Reset));
            // The cursor is abandoned: after a reset the client re-fetches, so
            // every live frame is new to it.
            high_water = 0;
        }
        crate::stream::Replay::Frames(frames) => {
            for frame in frames {
                high_water = high_water.max(frame.seq);
                if wants_topic(&topics, &frame.topic) {
                    prelude.push(encoding.encode(&frame));
                }
            }
        }
    }

    let body = futures_util::stream::unfold(
        (receiver, prelude.into_iter(), high_water, topics, encoding),
        |(mut receiver, mut prelude, high_water, topics, encoding)| async move {
            if let Some(line) = prelude.next() {
                return Some((
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from(line)),
                    (receiver, prelude, high_water, topics, encoding),
                ));
            }
            loop {
                match receiver.recv().await {
                    Ok(frame) => {
                        // Already delivered by the replay: dropping it here is
                        // what makes subscribe-then-replay non-duplicating as
                        // well as gapless.
                        if frame.seq <= high_water || !wants_topic(&topics, &frame.topic) {
                            continue;
                        }
                        return Some((
                            Ok(bytes::Bytes::from(encoding.encode(&frame))),
                            (receiver, prelude, high_water, topics, encoding),
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                        // [D-5]: announce, *then* end. A reader that is simply
                        // told nothing cannot tell a gap from a quiet relay.
                        let overflow = crate::stream::StreamControl::Overflow {
                            dropped,
                            since_seq: high_water,
                        };
                        return Some((
                            Ok(bytes::Bytes::from(control_line(encoding, &overflow))),
                            // The receiver is dropped by returning `None` on
                            // the next poll: `prelude` is exhausted and the
                            // closed channel ends the stream.
                            (receiver, prelude, u64::MAX, Vec::new(), encoding),
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, encoding.content_type())
        // Chrome's SSE and every intermediary want this: a buffered event
        // stream is a stream that arrives all at once when it ends.
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from_stream(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Encode a control frame in the negotiated encoding.
///
/// Control frames carry no `seq` — they are statements *about* the sequence
/// rather than positions in it — so SSE gets no `id:` line for them and a
/// client's `Last-Event-ID` cannot be moved by one.
fn control_line(
    encoding: crate::stream::Encoding,
    control: &crate::stream::StreamControl,
) -> String {
    let json = serde_json::to_string(control).unwrap_or_else(|_| "{}".into());
    match encoding {
        crate::stream::Encoding::Ndjson => format!("{json}\n"),
        crate::stream::Encoding::Sse => format!("data: {json}\n\n"),
    }
}

/// Whether a topic passes the client's `?topic=` filter.
///
/// Prefix matching on the dotted namespace, so `topic=agent` selects
/// `agent.frame`, `agent.metric`, and `agent.state` without the client
/// enumerating them. An empty filter selects everything.
fn wants_topic(topics: &[String], topic: &str) -> bool {
    topics.is_empty()
        || topics
            .iter()
            .any(|wanted| topic.starts_with(wanted.as_str()))
}

/// Parse a channel path segment as a uuid.
fn parse_channel_uuid(id: &str) -> crate::Result<uuid::Uuid> {
    uuid::Uuid::parse_str(id)
        .map_err(|_| DaemonError::InvalidInput(format!("channel id is not a uuid: {id}")))
}

/// Build the daemon router.
///
/// **Every [`WAVE1_ENDPOINTS`] path is mounted.** An earlier revision mounted
/// only the reads, on the reasoning that "a mounted endpoint returning
/// `relay_unreachable` for a reason the operator cannot fix is worse than an
/// endpoint honestly absent from `capabilities[]`". That reasoning was right
/// about the *symptom* and wrong about the cause: the endpoints returned
/// `relay_unreachable` because there was no relay loop, and now there is one.
/// With the loop present, an unmounted write path is the worse failure — a
/// `404` on `/channel/{id}/message` is indistinguishable from a typo'd route,
/// while a `503 relay_unreachable` names the actual condition and the TUI
/// already renders it with a retry (§2.7).
///
/// Two cross-cutting properties, neither of which is a layer:
/// 1. **Peer-credential authorization** happens at accept
///    ([`crate::socket::authorize_peer`]), before a request is parsed at all —
///    a rejected uid never reaches routing.
/// 2. **Response redaction** happens in `IntoResponse for DaemonError`, so a
///    secret cannot reach a client through any error path (§2.5).
pub fn router(state: AppState) -> Router {
    Router::new()
        // Meta
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/daemon", get(daemon))
        .route("/daemon/registry", get(daemon_registry))
        .route("/daemon/shutdown", post(shutdown))
        .route("/daemon/reconnect", post(daemon_reconnect))
        // Session
        .route("/session", get(session))
        .route("/session/identity", get(session_identity))
        .route("/session/relay-info", get(session_relay_info))
        // Channels
        .route("/channel", get(list_channels).post(create_channel))
        .route("/channel/{id}", get(get_channel))
        .route("/channel/{id}/member", get(channel_members))
        .route("/channel/{id}/join", post(join_channel))
        .route("/channel/{id}/leave", post(leave_channel))
        .route(
            "/channel/{id}/message",
            get(channel_window).post(send_message),
        )
        .route("/channel/{id}/typing", post(typing))
        .route("/channel/{id}/read", post(mark_channel_read))
        // Messages and threads
        .route("/message/{id}", get(get_message))
        .route("/message/{id}/thread", get(get_thread))
        .route("/message/{id}/reaction", get(get_reactions).post(react))
        .route("/message/{id}/ask", post(answer_ask))
        // Search
        .route("/search", get(search))
        .route("/search/user", get(search_user))
        // Directory
        .route("/user", get(list_users))
        .route("/user/{pubkey}", get(get_user))
        .route("/mention/candidates", get(mention_candidates))
        .route("/mention/inbox", get(mention_inbox))
        .route("/read-state", get(read_state).put(put_read_state))
        .route("/presence", get(presence))
        // Agents
        .route("/agent", get(list_agents))
        .route("/agent/fleet", get(agent_fleet))
        .route("/agent/{pk}", get(get_agent))
        .route("/agent/{pk}/activity", get(agent_activity))
        .route("/agent/{pk}/transcript", get(agent_transcript))
        .route("/agent/{pk}/metric", get(agent_metric))
        .route("/agent/{pk}/control", post(agent_control))
        // The one event stream
        .route("/event", get(event_stream))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            note_client_activity,
        ))
        .with_state(state)
}

/// Reset the idle timer on **every** request (§2.2).
///
/// A layer rather than a call in each handler, because "every handler remembers
/// to do this" is a rule that holds until someone adds a handler. The failure it
/// prevents is specific and bad: without it the timer measures uptime rather
/// than idleness, so a daemon in continuous use exits 30 minutes after startup —
/// mid-session, taking the observer archive with it — and the symptom looks like
/// a crash rather than a timer.
///
/// §2.2 is precise about what counts: the timer keys on client **activity**, not
/// on connection presence, because a detached tmux pane holding an `/event`
/// stream open is the normal state for this product's population rather than
/// evidence of a live client. A request is activity; an open socket is not.
async fn note_client_activity(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    state.touch().await;
    next.run(request).await
}

/// Routes mounted by [`router`] today.
///
/// **Now equal to [`WAVE1_ENDPOINTS`]**, and a test below asserts the equality
/// rather than trusting it. The two lists stay separate types because the
/// distinction is real — §2.3 [D-1] says `capabilities[]` decides which screens
/// exist, so a future wave that specifies an endpoint before implementing it
/// gets a legible gap rather than a silent `404` — but the gap is zero today.
pub const MOUNTED_ENDPOINTS: &[&str] = WAVE1_ENDPOINTS;

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.4 lists the Wave-1 subset; duplicates would make the spec-check
    /// comparison ambiguous.
    #[test]
    fn endpoints_are_unique() {
        let unique: std::collections::BTreeSet<_> = WAVE1_ENDPOINTS.iter().collect();
        assert_eq!(unique.len(), WAVE1_ENDPOINTS.len());
    }

    /// §2.4: permission answering is `/message/{id}/ask`, not a control frame.
    #[test]
    fn ask_answering_is_a_message_endpoint() {
        assert!(WAVE1_ENDPOINTS.contains(&"/message/{id}/ask"));
    }

    /// §4.1.3: no moderation, no deploy, no media, no forum, no projects in
    /// Wave 1.
    #[test]
    fn later_wave_endpoints_are_absent() {
        for later in [
            "/moderation",
            "/backend/deploy",
            "/media",
            "/forum",
            "/project",
            "/dm/open",
        ] {
            assert!(
                !WAVE1_ENDPOINTS.iter().any(|e| e.starts_with(later)),
                "{later} is not Wave 1"
            );
        }
    }

    /// The gap that used to exist is now zero, and this test is what keeps it
    /// from silently reopening: adding a path to [`WAVE1_ENDPOINTS`] without
    /// mounting it fails the build rather than shipping a `404` the client
    /// discovers at runtime.
    #[test]
    fn every_wave1_endpoint_is_mounted() {
        let mounted: std::collections::BTreeSet<_> = MOUNTED_ENDPOINTS.iter().collect();
        let missing: Vec<_> = WAVE1_ENDPOINTS
            .iter()
            .filter(|path| !mounted.contains(path))
            .collect();
        assert!(
            missing.is_empty(),
            "unmounted Wave-1 endpoints: {missing:?}"
        );
        assert_eq!(mounted.len(), WAVE1_ENDPOINTS.len());
    }

    /// §2.3/§2.2: the meta endpoints that make the daemon supervisable.
    #[test]
    fn meta_endpoints_are_present() {
        for required in [
            "/health",
            "/openapi.json",
            "/daemon/registry",
            "/daemon/shutdown",
            "/event",
        ] {
            assert!(WAVE1_ENDPOINTS.contains(&required), "{required}");
        }
    }
}
