//! End-to-end tests over the real Unix socket.
//!
//! These bind an actual `UnixListener`, serve the actual router, and speak
//! HTTP/1.1 over it — because the properties under test are ones a
//! `Router::oneshot` cannot observe. `DESIGN.md` §2.5 makes filesystem
//! permissions the authorization model and §2.3 makes the socket the liveness
//! probe; both are statements about a *socket*, and asserting them against an
//! in-memory service asserts nothing.
//!
//! The client is hand-rolled rather than `hyper`-driven for one reason: it must
//! be able to send a deliberately malformed request. A well-behaved client
//! library cannot.

use std::path::Path;

use buzz_daemon::config::{Config, SocketIdentity};
use buzz_daemon::identity::Identity;
use buzz_daemon::state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A daemon bound to a temporary socket, serving for the test's lifetime.
struct Harness {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    state: AppState,
}

impl Harness {
    async fn start(identity: Option<Identity>) -> Self {
        Self::start_with(identity, |_| {}).await
    }

    /// Start a daemon, letting the caller seed its state before it serves.
    async fn start_with(
        identity: Option<Identity>,
        seed: impl FnOnce(&mut buzz_daemon::state::Inner),
    ) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let config = Config {
            identity: SocketIdentity::new("wss://relay.example", "aa".repeat(32), ""),
            socket: socket.clone(),
            runtime_dir: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            idle_timeout: None,
            observer_cache_bytes: 1024 * 1024,
            systemd_managed: false,
        };
        let state = AppState::new(config, identity).expect("state");
        seed(&mut *state.lock().await);

        let listener = buzz_daemon::socket::bind(&socket).expect("bind");
        let app = buzz_daemon::api::router(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // The socket is bound before `serve` is polled, so a connect can race
        // the accept loop's first poll. Waiting on a real connect rather than
        // sleeping keeps the test deterministic on a loaded machine.
        for _ in 0..100 {
            if UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        Self {
            _dir: dir,
            socket,
            state,
        }
    }

    /// Send a raw HTTP/1.1 request and return `(status, body)`.
    async fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let raw = self
            .raw(&format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            ))
            .await;
        parse_response(&raw)
    }

    async fn post(&self, path: &str) -> (u16, serde_json::Value) {
        let raw = self
            .raw(&format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ))
            .await;
        parse_response(&raw)
    }

    async fn raw(&self, request: &str) -> String {
        let mut stream = UnixStream::connect(&self.socket).await.expect("connect");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Read the body of `GET /event` up to a short quiet period.
    ///
    /// `read_to_end` cannot be used here and that is the point: `/event` never
    /// closes — it is an open stream, which is what makes it the *one* push
    /// channel. Reading until the daemon stops writing is what a real client
    /// does with a line reader, and it is the only way to assert on a stream's
    /// contents without asserting that it ended.
    async fn stream_body(&self, path: &str) -> String {
        let raw = self.stream_raw(path, "application/x-ndjson").await;
        dechunk(raw.split("\r\n\r\n").nth(1).unwrap_or(""))
    }

    /// The full response — headers included — of a streaming request.
    async fn stream_raw(&self, path: &str, accept: &str) -> String {
        let mut stream = UnixStream::connect(&self.socket).await.expect("connect");
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: {accept}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("write request");

        let mut response = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            // The quiet period is what terminates the read: the replayed
            // prelude arrives immediately and the live half is silent in a test
            // with no relay, so 250 ms of nothing means the prelude is complete.
            let read = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                stream.read(&mut buffer),
            )
            .await;
            match read {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => response.extend_from_slice(&buffer[..n]),
                Ok(Err(err)) => panic!("stream read failed: {err}"),
            }
        }
        String::from_utf8_lossy(&response).into_owned()
    }
}

fn parse_response(raw: &str) -> (u16, serde_json::Value) {
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in {raw:?}"));
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    // A body-less response (204, or an error the server closed on) is not a
    // parse failure — returning null keeps the assertion at the call site.
    let value = serde_json::from_str(body.trim()).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// Strip HTTP/1.1 chunked framing from a streamed body.
///
/// Not incidental plumbing: a body with no `Content-Length` — which is every
/// open stream, by definition — is chunked, so `{"seq":1,…}` arrives on the
/// wire as `1f\r\n{"seq":1,…}\n\r\n`. A test that parses the raw body sees the
/// hex length prefix and fails with `expected value at line 1 column 1`, which
/// reads like the daemon emitted malformed JSON rather than like the test
/// forgot a transfer encoding. Doing it here, once, is what keeps that
/// misdiagnosis out of every streaming assertion.
///
/// A hand-rolled de-chunker rather than a client library for the same reason
/// the rest of this harness is hand-rolled: `hyper` would also hide the framing
/// bug this exists to make visible.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some((size_line, remainder)) = rest.split_once("\r\n") else {
            // No framing at all: a short or unframed body is passed through
            // rather than discarded, so a regression to `Content-Length` shows
            // up as a *content* assertion failing rather than as an empty
            // string that could mean anything.
            out.push_str(rest);
            break;
        };
        let Ok(size) = usize::from_str_radix(size_line.trim(), 16) else {
            out.push_str(rest);
            break;
        };
        if size == 0 || remainder.len() < size {
            out.push_str(&remainder[..remainder.len().min(size)]);
            break;
        }
        out.push_str(&remainder[..size]);
        rest = remainder[size..].trim_start_matches("\r\n");
    }
    out
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("socket exists")
        .permissions()
        .mode()
        & 0o777
}

// ── §2.5 the socket posture ───────────────────────────────────────────────

/// §2.5: `0600` on the socket, and it is `0600` **at bind** rather than
/// chmod-ed afterwards — the window between the two is connectable by every uid
/// on a box that, per §1.2, is running arbitrary agents under other uids.
#[tokio::test]
async fn the_served_socket_is_owner_only() {
    let harness = Harness::start(None).await;
    assert_eq!(mode_of(&harness.socket), 0o600);
}

/// No TCP listener ships (§2.5). The whole transport story is one socket file.
#[tokio::test]
async fn the_daemon_answers_over_the_unix_socket() {
    let harness = Harness::start(None).await;
    let (status, body) = harness.get("/health").await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
}

// ── §2.3 [D-1] /health and the capability floor ───────────────────────────

/// §2.3 [D-1]: `/health` carries `{version, api_version, capabilities[]}` —
/// the floor check plus the capability presence that decides which screens
/// exist.
#[tokio::test]
async fn health_carries_the_version_floor_and_capabilities() {
    let harness = Harness::start(None).await;
    let (_, body) = harness.get("/health").await;
    assert_eq!(body["api_version"], buzz_daemon::API_VERSION);
    assert_eq!(body["version"], buzz_daemon::VERSION);
    let caps = body["capabilities"].as_array().expect("capabilities array");
    assert!(caps.iter().any(|c| c == "channels"), "{body}");
    assert!(caps.iter().any(|c| c == "agents"), "{body}");
    assert!(body["uptime_secs"].is_number());
}

/// §2.5: **keyless is a visible state, not a quiet one.** A keyless daemon must
/// never look identical to a healthy one — this is the field that makes the
/// difference observable from outside the process.
#[tokio::test]
async fn a_keyless_daemon_reports_archiving_false() {
    let keyless = Harness::start(None).await;
    let (_, body) = keyless.get("/health").await;
    assert_eq!(body["archiving"], serde_json::json!(false));

    let keyed = Harness::start(Some(Identity::from_keys(nostr::Keys::generate(), None))).await;
    let (_, body) = keyed.get("/health").await;
    assert_eq!(body["archiving"], serde_json::json!(true));
}

// ── §4.1.4 criterion 5: the counters ──────────────────────────────────────

/// Exit criterion 5 reads `GET /daemon`'s drop counters across a working day.
/// They are only a usable criterion if every guard has its own, and if the
/// endpoint actually serves them.
#[tokio::test]
async fn the_daemon_endpoint_exposes_every_drop_counter() {
    let owner = nostr::Keys::generate();
    let agent = nostr::Keys::generate();
    let identity = Identity::from_keys(owner.clone(), None);
    let ingest_identity = Identity::from_keys(owner.clone(), None);
    let now = 1_785_852_720i64;

    // A genuinely stale frame, so the counter moves through the real guard
    // chain rather than by a test reaching in and incrementing it. A counter a
    // test can set directly is a counter the production path might never touch.
    let payload =
        serde_json::json!({"seq": 1, "timestamp": "t", "kind": "acp_read", "payload": {}});
    let ciphertext =
        buzz_core::observer::encrypt_observer_payload(&agent, &owner.public_key(), &payload)
            .unwrap();
    let stale = nostr::EventBuilder::new(
        nostr::Kind::Custom(buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16),
        ciphertext,
    )
    .tags([nostr::Tag::parse([
        buzz_core::observer::OBSERVER_AGENT_TAG,
        &agent.public_key().to_hex(),
    ])
    .unwrap()])
    .custom_created_at(nostr::Timestamp::from_secs(
        (now - buzz_daemon::observer::OBSERVER_FRESHNESS_SECS - 60) as u64,
    ))
    .sign_with_keys(&agent)
    .unwrap();

    let harness = Harness::start_with(Some(identity), move |inner| {
        // Two *different* counters, so the response cannot pass by reporting a
        // single total.
        inner.session.record_pong_timeout();
        inner
            .observer
            .register_agent(agent.public_key().to_hex(), &ingest_identity, now);
        assert_eq!(
            inner.observer.ingest(&stale, &ingest_identity, now),
            buzz_daemon::observer::Ingest::Dropped(buzz_daemon::observer::Guard::Freshness)
        );
    })
    .await;

    let (status, body) = harness.get("/daemon").await;
    assert_eq!(status, 200);
    assert_eq!(body["session_counters"]["pong_timeouts"], 1);
    assert_eq!(
        body["observer_counters"]["dropped"]["observer_dropped_stale"], 1,
        "a guard-0 drop must be attributable to guard 0, not to a total"
    );
    // §2.2: the full preimage is on the pidfile *and* here, so a socket-path
    // collision is diagnosable by reading it rather than by guessing.
    assert_eq!(body["preimage"], harness.state.config.identity.preimage());
    assert!(body["pid"].is_number());
}

// ── §2.6: the connection state, surfaced verbatim ─────────────────────────

/// §2.6: the states are surfaced **verbatim** because they look identical to
/// "hung" if collapsed. `/session` is where the status bar reads them.
#[tokio::test]
async fn the_session_endpoint_surfaces_the_connection_state_verbatim() {
    let harness = Harness::start(None).await;
    harness.state.lock().await.session.transition(
        buzz_daemon::session::ConnectionState::Reconnecting {
            attempt: 3,
            next_retry_in_ms: 4_000,
        },
    );
    let (_, body) = harness.get("/session").await;
    assert_eq!(body["connection"]["state"], "reconnecting");
    assert_eq!(body["connection"]["attempt"], 3);
    assert_eq!(body["connection"]["next_retry_in_ms"], 4_000);
}

/// §1.3 property 3: auth failure is textually distinct from network failure,
/// with its cause carried so the TUI can offer the right remediation.
#[tokio::test]
async fn auth_failure_reaches_the_client_as_its_own_state() {
    let harness = Harness::start(None).await;
    harness.state.lock().await.session.transition(
        buzz_daemon::session::ConnectionState::AuthFailed {
            reason: "oa_expired".into(),
        },
    );
    let (_, body) = harness.get("/session").await;
    assert_eq!(body["connection"]["state"], "auth_failed");
    assert_eq!(body["connection"]["reason"], "oa_expired");
}

// ── The error model ───────────────────────────────────────────────────────

/// §2.4/§3.13: one error shape everywhere, with a stable `code` the front end
/// switches on. A handler that formatted its own would be a code the client has
/// to special-case.
#[tokio::test]
async fn a_missing_entity_returns_the_standard_error_shape() {
    let harness = Harness::start(None).await;
    let (status, body) = harness.get("/channel/does-not-exist").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["error"]["message"].is_string(), "{body}");
}

/// An unmounted Wave-1 endpoint 404s rather than hanging or 500-ing. §2.3
/// [D-1] is explicit that a client above the floor **hides** what
/// `capabilities[]` does not claim, so reaching one is a client bug and gets a
/// clean answer.
#[tokio::test]
async fn an_unmounted_endpoint_is_a_clean_404() {
    let harness = Harness::start(None).await;
    let (status, _) = harness.get("/moderation/audit").await;
    assert_eq!(status, 404);
}

// ── Reads served from the cache ───────────────────────────────────────────

/// `NAVIGATION.md` §1.1/§2.1: the channel list is attention-first and carries
/// the ambient state — unread, mentions, and *where agents are working* — so
/// the L1 layer and the drawer cannot disagree.
#[tokio::test]
async fn the_channel_list_is_attention_first_and_carries_ambient_state() {
    let quiet = uuid::Uuid::from_bytes([1; 16]).to_string();
    let loud = uuid::Uuid::from_bytes([2; 16]).to_string();
    let harness = Harness::start_with(None, |inner| {
        for (id, name) in [(&quiet, "zebra"), (&loud, "alpha")] {
            let mut channel = buzz_daemon::channels::Channel::unknown(id);
            channel.name = name.into();
            inner.channels.upsert(channel);
        }
        inner.channels.set_unread(&loud, 4, 2);
        inner.channels.register_agent("pk-claude", "claude-1");
        inner.channels.set_agent_working("pk-claude", Some(&quiet));
    })
    .await;

    let (status, body) = harness.get("/channel").await;
    assert_eq!(status, 200);
    let channels = body["channels"].as_array().expect("array");
    assert_eq!(
        channels[0]["name"], "alpha",
        "the mentioned channel leads, despite sorting last by name"
    );
    assert_eq!(
        channels[1]["agents_working"],
        serde_json::json!(["claude-1"])
    );
    assert_eq!(body["totals"]["unread"], 4);
    assert_eq!(body["totals"]["mentions"], 2);
}

/// [D-2]: every candidate carries its **resolved pubkey**, which is what makes
/// "what you picked is what gets tagged" true by construction. The cap ships
/// with them so the composer's live `n of 50` counter needs no hardcoded number.
#[tokio::test]
async fn mention_candidates_carry_pubkeys_and_the_cap() {
    let channel = uuid::Uuid::from_bytes([3; 16]).to_string();
    let pubkey = "ab".repeat(32);
    let harness = Harness::start_with(None, |inner| {
        inner
            .channels
            .upsert(buzz_daemon::channels::Channel::unknown(&channel));
        inner
            .channels
            .set_roster(&channel, [pubkey.clone()].into_iter().collect());
        inner.mentions.upsert(buzz_daemon::mentions::Profile {
            pubkey: pubkey.clone(),
            display_name: Some("matt".into()),
            ..Default::default()
        });
    })
    .await;

    let (status, body) = harness
        .get(&format!("/mention/candidates?channel={channel}&prefix=ma"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["candidates"][0]["pubkey"], pubkey);
    assert_eq!(body["candidates"][0]["in_roster"], true);
    assert_eq!(body["cap"], buzz_daemon::mentions::MENTION_CAP);
}

/// §2.4: `unknown` is a distinct state, and `GET /presence` answers for a
/// pubkey it has never heard of rather than omitting it — an absent key in the
/// map is indistinguishable from a dropped request.
#[tokio::test]
async fn presence_answers_unknown_rather_than_omitting() {
    let harness = Harness::start(None).await;
    let (status, body) = harness.get("/presence?pubkeys=never-seen").await;
    assert_eq!(status, 200);
    assert_eq!(body["presence"]["never-seen"]["state"], "unknown");
    assert_eq!(body["presence"]["never-seen"]["source"], "none");
}

/// §3.4: the fleet is sorted blocked-first, always, and that ordering reaches
/// the client rather than being a client concern.
#[tokio::test]
async fn the_fleet_endpoint_serves_blocked_first() {
    let harness = Harness::start_with(None, |inner| {
        let working = inner.fleet.agent_mut("pk-working");
        working.name = "working".into();
        working.presence = Some(buzz_daemon::presence::Presence::Present);
        working.turn = Some("t".into());

        let blocked = inner.fleet.agent_mut("pk-blocked");
        blocked.name = "blocked".into();
        blocked.presence = Some(buzz_daemon::presence::Presence::Present);
        blocked.awaiting_answer = true;
    })
    .await;

    let (status, body) = harness.get("/agent/fleet").await;
    assert_eq!(status, 200);
    assert_eq!(body["agents"][0]["state"], "blocked");
    assert_eq!(body["agents"][1]["state"], "working");
}

/// `daemon-api.md` §3.9: activity is **one** sorted deduplicated sequence, so a
/// tool-call start in the archive pairs with its end in the live ring.
#[tokio::test]
async fn agent_activity_serves_a_single_ordered_sequence() {
    let owner = nostr::Keys::generate();
    let agent = nostr::Keys::generate();
    let agent_pubkey = agent.public_key().to_hex();
    let identity = Identity::from_keys(owner.clone(), None);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let frames: Vec<nostr::Event> = (1..=3)
        .map(|seq| {
            let payload = serde_json::json!({
                "seq": seq,
                "timestamp": "2026-08-04T14:12:00Z",
                "kind": "acp_read",
                "payload": {},
            });
            let ciphertext = buzz_core::observer::encrypt_observer_payload(
                &agent,
                &owner.public_key(),
                &payload,
            )
            .unwrap();
            nostr::EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16),
                ciphertext,
            )
            .tags([
                nostr::Tag::parse([buzz_core::observer::OBSERVER_AGENT_TAG, &agent_pubkey])
                    .unwrap(),
            ])
            .custom_created_at(nostr::Timestamp::from_secs(now as u64))
            .sign_with_keys(&agent)
            .unwrap()
        })
        .collect();

    let ingest_identity = Identity::from_keys(owner, None);
    let pubkey_for_seed = agent_pubkey.clone();
    let harness = Harness::start_with(Some(identity), move |inner| {
        inner
            .observer
            .register_agent(&pubkey_for_seed, &ingest_identity, now);
        for frame in &frames {
            inner.observer.ingest(frame, &ingest_identity, now);
        }
    })
    .await;

    let (status, body) = harness
        .get(&format!("/agent/{agent_pubkey}/activity"))
        .await;
    assert_eq!(status, 200);
    let seqs: Vec<u64> = body["frames"]
        .as_array()
        .expect("frames")
        .iter()
        .map(|f| f["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [1, 2, 3]);
}

// ── Lifecycle ─────────────────────────────────────────────────────────────

/// §2.2: `/daemon/shutdown` answers **before** exiting. A client that gets a
/// connection reset cannot tell "shut down" from "crashed", and §1.3 property 3
/// applies to the daemon's own lifecycle too.
#[tokio::test]
async fn shutdown_answers_before_it_exits() {
    let harness = Harness::start(None).await;
    let (status, body) = harness.post("/daemon/shutdown").await;
    assert_eq!(status, 200);
    assert_eq!(body["stopping"], true);
    assert_eq!(body["systemd_managed"], false);
    // The harness's spawned exit would take the test process with it, so the
    // handler's 50 ms grace is deliberately not waited out here. The assertion
    // that matters is that the response arrived at all.
}

/// A malformed request line is a clean rejection, not a panic that takes the
/// accept loop down with it. Worth an explicit test because the client is a
/// terminal front end, and a front end with a bug must not be able to kill the
/// daemon holding the observer archive.
#[tokio::test]
async fn a_malformed_request_does_not_kill_the_daemon() {
    let harness = Harness::start(None).await;
    let _ = harness.raw("this is not HTTP\r\n\r\n").await;
    let (status, _) = harness.get("/health").await;
    assert_eq!(status, 200, "the daemon still serves after a bad request");
}

/// §2.2: the idle timer keys on client **activity**, and every request is
/// activity — the reset happens in a layer rather than per-handler.
///
/// Regression test for a real defect: `touch()` existed and was called from
/// nothing, so the timer measured **uptime** instead of idleness. A daemon in
/// continuous use would have exited 30 minutes after startup, mid-session,
/// taking the observer archive with it — and the symptom would have looked like
/// a crash rather than a timer.
#[tokio::test]
async fn every_request_resets_the_idle_timer() {
    let harness = Harness::start(None).await;
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    let before = harness.state.idle_for().await;
    assert!(
        before >= std::time::Duration::from_millis(40),
        "the timer must actually advance while nothing happens"
    );

    let (status, _) = harness.get("/health").await;
    assert_eq!(status, 200);
    assert!(
        harness.state.idle_for().await < before,
        "a request must reset the idle timer, or it measures uptime"
    );
}

/// The reset covers **every** mounted route, not just the one somebody
/// remembered. That is the property the layer buys over a per-handler call, and
/// asserting it is what keeps a future route from silently opting out.
///
/// The assertion is deliberately *absolute* rather than "smaller than before":
/// a relative comparison races the scheduler under a parallel test run, where a
/// request can take longer than the settle window and make a working reset look
/// like a failure. Asserting the timer sits **below the settle window** after a
/// request is the same property stated in a way that cannot flake — it is false
/// exactly when the reset did not happen.
#[tokio::test]
async fn the_idle_reset_covers_every_mounted_route() {
    /// Long enough that a scheduling hiccup cannot exceed it, short enough that
    /// a genuinely missing reset (which leaves the timer at test-lifetime
    /// scale) always trips it.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

    let harness = Harness::start(None).await;
    for path in [
        "/health",
        "/openapi.json",
        "/daemon",
        "/session",
        "/channel",
        "/mention/candidates?channel=x",
        "/presence?pubkeys=x",
        "/read-state",
        "/agent/fleet",
    ] {
        tokio::time::sleep(SETTLE).await;
        assert!(
            harness.state.idle_for().await >= SETTLE,
            "the timer must advance while nothing happens"
        );
        harness.get(path).await;
        let after = harness.state.idle_for().await;
        assert!(
            after < SETTLE,
            "{path} did not reset the idle timer (idle_for = {after:?})"
        );
    }
}

// ── The endpoints the relay loop unblocked ─────────────────────────────────
//
// These bind the *whole* Wave-1 surface now that `wire.rs` exists. The
// harness starts a daemon with **no relay loop** (`state.wire` is `None`),
// which is exactly the interesting case: the honest answer to a write with no
// relay is `503 relay_unreachable`, not `404`. A `404` is indistinguishable
// from a typo'd route; a `503` names the condition and the TUI already renders
// it with a retry (§2.7).

/// Every mounted route answers *something*. A route that hangs or 500s on a
/// bare request is worse than one that is absent, because a client cannot tell
/// it from a wedged daemon.
#[tokio::test]
async fn every_mounted_read_endpoint_answers() {
    let harness = Harness::start(None).await;
    for path in [
        "/health",
        "/openapi.json",
        "/daemon",
        "/daemon/registry",
        "/session",
        "/session/identity",
        "/channel",
        "/read-state",
        "/presence",
        "/agent",
        "/agent/fleet",
    ] {
        let (status, _) = harness.get(path).await;
        assert!(
            status < 500,
            "{path} answered {status}; a mounted route must not 500 on a bare GET"
        );
    }
}

/// §2.7: a write with no relay is `503 relay_unreachable`, with the `code` the
/// TUI switches on. This is the whole reason mounting the write endpoints is
/// now the right call rather than the wrong one — the endpoint exists and names
/// its own condition.
#[tokio::test]
async fn a_write_without_a_relay_loop_is_relay_unreachable_not_a_404() {
    let identity = Identity::from_keys(nostr::Keys::generate(), None);
    let harness = Harness::start(Some(identity)).await;
    let (status, body) = harness
        .post("/channel/3f1d9c9e-0f7a-4a2e-9b1f-2c4d5e6f7a8b/join")
        .await;
    assert_ne!(status, 404, "the route must be mounted");
    assert_eq!(body["error"]["code"], "relay_unreachable", "{body}");
}

/// `daemon-api.md` §3.5: typing is **always `202`**, never an error. It is
/// dropped rather than queued under the rate-limit gate, so an error the TUI
/// has to handle would be an error about a frame that is allowed to vanish.
#[tokio::test]
async fn typing_is_always_accepted_even_with_no_relay() {
    let identity = Identity::from_keys(nostr::Keys::generate(), None);
    let harness = Harness::start(Some(identity)).await;
    let (status, _) = harness
        .post("/channel/3f1d9c9e-0f7a-4a2e-9b1f-2c4d5e6f7a8b/typing")
        .await;
    assert_eq!(status, 202);
}

/// §2.4: `/openapi.json` must describe what is actually mounted. An empty
/// `paths` object tells a generated client the daemon has no API at all.
#[tokio::test]
async fn the_openapi_document_lists_the_mounted_paths() {
    let harness = Harness::start(None).await;
    let (status, body) = harness.get("/openapi.json").await;
    assert_eq!(status, 200);
    let paths = body["paths"].as_object().expect("paths is an object");
    for endpoint in buzz_daemon::api::MOUNTED_ENDPOINTS {
        assert!(
            paths.contains_key(*endpoint),
            "{endpoint} missing from the spec"
        );
    }
}

/// §4.1.1 deliverable 13: `GET /event` streams ndjson by default, and every
/// frame already in the ring is replayed from `?since=0`.
#[tokio::test]
async fn the_event_stream_replays_the_ring_as_ndjson() {
    let harness = Harness::start_with(None, |inner| {
        inner.stream.publish(
            "message.new",
            serde_json::json!({"channel_id": "c", "n": 1}),
        );
        inner.stream.publish(
            "message.new",
            serde_json::json!({"channel_id": "c", "n": 2}),
        );
    })
    .await;

    let raw = harness.stream_body("/event?since=0").await;
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(lines.len() >= 2, "expected the ring replayed, got {raw:?}");
    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("ndjson line");
    assert_eq!(first["type"], "message.new");
    assert_eq!(first["seq"], 1);
    assert_eq!(first["payload"]["n"], 1);
}

/// §2.6 link A: a cursor that has aged out of the ring gets `stream.reset`
/// **first**, so the TUI invalidates and re-fetches rather than presenting a
/// silently gapped timeline.
#[tokio::test]
async fn an_aged_out_cursor_gets_stream_reset_first() {
    let harness = Harness::start_with(None, |inner| {
        inner.stream.publish("message.new", serde_json::json!({}));
    })
    .await;

    // A cursor ahead of the sequence is from a previous daemon process whose
    // numbering started over — the numbers do not refer to the same events.
    let raw = harness.stream_body("/event?since=999999").await;
    let first = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let parsed: serde_json::Value = serde_json::from_str(first).expect("ndjson line");
    assert_eq!(parsed["type"], "stream.reset", "{raw:?}");
}

/// The `?topic=` filter matches on the dotted namespace by prefix, so a client
/// asking for `agent` gets `agent.frame` and `agent.metric` without
/// enumerating them — and does not get `message.new`.
#[tokio::test]
async fn the_topic_filter_selects_by_dotted_prefix() {
    let harness = Harness::start_with(None, |inner| {
        inner.stream.publish("message.new", serde_json::json!({}));
        inner.stream.publish("agent.frame", serde_json::json!({}));
        inner.stream.publish("agent.metric", serde_json::json!({}));
    })
    .await;

    let raw = harness.stream_body("/event?since=0&topic=agent").await;
    let types: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["type"].as_str().map(str::to_string))
        .collect();
    assert!(types.contains(&"agent.frame".to_string()), "{types:?}");
    assert!(types.contains(&"agent.metric".to_string()), "{types:?}");
    assert!(!types.contains(&"message.new".to_string()), "{types:?}");
}

/// SSE is content-negotiated and carries `id:` with the same `seq` the body
/// does, so an SSE client's own `Last-Event-ID` reconnect and the daemon's
/// `?since=` cursor are one number rather than two schemes to reconcile.
#[tokio::test]
async fn sse_is_negotiated_and_its_id_matches_the_body_seq() {
    let harness = Harness::start_with(None, |inner| {
        inner.stream.publish("message.new", serde_json::json!({}));
    })
    .await;

    let raw = harness
        .stream_raw("/event?since=0", "text/event-stream")
        .await;
    assert!(raw.contains("text/event-stream"), "{raw}");
    assert!(raw.contains("id: 1"), "{raw}");
    assert!(raw.contains("\"seq\":1"), "{raw}");
}

/// **M3 regression.** [D-5]'s "announce, **then** end" must actually end.
///
/// The `Lagged` arm used to emit `stream.overflow`, set `high_water = u64::MAX`,
/// and carry a comment asserting the next poll would return `None` because "the
/// closed channel ends the stream". That premise is false: the broadcast
/// `Sender` lives in `Inner.stream` for the life of the process, so
/// `RecvError::Closed` — the only `return None` path — is unreachable while the
/// daemon runs. What happened instead was that `prelude` was exhausted, `recv()`
/// kept succeeding, and every frame failed `seq <= u64::MAX`, so the loop
/// `continue`d forever: the reader got exactly one overflow line and then a
/// connection that stayed open, consumed every subsequent frame, and emitted
/// nothing ever again — indistinguishable from a quiet relay, which is the
/// "looks alive while it is dead" failure [D-5] exists to prevent.
///
/// The assertion is therefore on the **close**, not on the announcement: an
/// announcement followed by a live connection is precisely the bug.
///
/// Nothing exercised this path before, because reaching it needs more than
/// `EVENT_BROADCAST_CAPACITY` frames published while a reader is stalled, and
/// every other streaming test publishes fewer than five.
#[tokio::test]
async fn an_overflowed_reader_is_disconnected_rather_than_silently_stalled() {
    let harness = Harness::start(None).await;

    let mut stream = UnixStream::connect(&harness.socket).await.expect("connect");
    stream
        .write_all(
            b"GET /event HTTP/1.1\r\nHost: localhost\r\nAccept: application/x-ndjson\r\n\r\n",
        )
        .await
        .expect("write request");

    // Read the headers so the handler has certainly subscribed before the
    // flood: publishing first would age the frames out of the ring instead of
    // lagging the receiver, which is a different path.
    let mut head = [0u8; 512];
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut head))
        .await
        .expect("headers arrive")
        .expect("headers read");

    // Overrun the broadcast buffer without reading a byte of the body.
    {
        let mut inner = harness.state.lock().await;
        for n in 0..(buzz_daemon::stream::EVENT_BROADCAST_CAPACITY * 2) {
            inner
                .stream
                .publish("message.new", serde_json::json!({"n": n}));
        }
    }

    // Read until the body ends. **The terminating chunk is the signal, not a
    // socket close.** `/event` is chunked by definition (no `Content-Length` —
    // it never has a known length), and HTTP/1.1 keep-alive means hyper may
    // hold the connection open for a subsequent request after the body is
    // complete. Asserting on close therefore fails against a *correct* server,
    // which is what the first draft of this test did — and the misdiagnosis it
    // invites ("the fix does not work") costs far more than these two lines.
    let mut body = Vec::new();
    let mut buffer = [0u8; 8192];
    let ended = loop {
        if body.ends_with(b"0\r\n\r\n") {
            break true;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut buffer))
            .await
        {
            // A close is also a legitimate end of body.
            Ok(Ok(0)) | Ok(Err(_)) => break true,
            Ok(Ok(n)) => body.extend_from_slice(&buffer[..n]),
            // Ten seconds of nothing with the body unterminated: the connection
            // is open and mute, which is the defect this test exists for.
            Err(_) => break false,
        }
    };

    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("stream.overflow"),
        "the loss must be announced, not silent: {text:?}"
    );
    assert!(
        ended,
        "the stream must end after announcing overflow; a body that never \
         terminates is indistinguishable from a quiet relay"
    );
}
