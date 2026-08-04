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

/// Build the daemon router.
///
/// Every route is a **read** in this revision. The write paths
/// ([`crate::post`], read-state publish, ask answers) are built and unit-tested
/// but not mounted, because mounting them requires the relay session's I/O half
/// — and a mounted endpoint that returns `relay_unreachable` for a reason the
/// operator cannot fix is worse than an endpoint that is honestly absent from
/// `capabilities[]`.
///
/// Two cross-cutting properties, neither of which is a layer:
/// 1. **Peer-credential authorization** happens at accept
///    ([`crate::socket::authorize_peer`]), before a request is parsed at all —
///    a rejected uid never reaches routing.
/// 2. **Response redaction** happens in `IntoResponse for DaemonError`, so a
///    secret cannot reach a client through any error path (§2.5).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/daemon", get(daemon))
        .route("/daemon/shutdown", post(shutdown))
        .route("/session", get(session))
        .route("/channel", get(list_channels))
        .route("/channel/{id}", get(get_channel))
        .route("/channel/{id}/member", get(channel_members))
        .route("/mention/candidates", get(mention_candidates))
        .route("/presence", get(presence))
        .route("/read-state", get(read_state))
        .route("/agent/fleet", get(agent_fleet))
        .route("/agent/{pk}/activity", get(agent_activity))
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
/// A strict subset of [`WAVE1_ENDPOINTS`], and the gap is deliberate: §2.3
/// [D-1] says `capabilities[]` decides which screens exist, so a client
/// attached to this build **hides** what it cannot reach rather than erroring
/// inside it. Keeping the two lists separate is what makes the gap legible
/// instead of looking like an oversight.
pub const MOUNTED_ENDPOINTS: &[&str] = &[
    "/health",
    "/openapi.json",
    "/daemon",
    "/daemon/shutdown",
    "/session",
    "/channel",
    "/channel/{id}",
    "/channel/{id}/member",
    "/mention/candidates",
    "/presence",
    "/read-state",
    "/agent/fleet",
    "/agent/{pk}/activity",
];

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
