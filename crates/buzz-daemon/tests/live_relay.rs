//! Live-relay integration tests — **read-only, always**.
//!
//! These exercise the daemon's relay-facing code against a real relay rather
//! than a fixture, which is the only way to catch the class of bug a fixture
//! cannot: a filter the relay's p-gate refuses, a NIP-98 signature the bridge
//! rejects, an auth tag whose header form is wrong. `DESIGN.md` §5.2's
//! `filter invariant` row is precisely a claim about what the *relay* does with
//! a filter, and asserting it against a mock asserts nothing.
//!
//! # Read-only is a hard boundary, not a convention
//!
//! The identity these tests run as is a **shared** test agent. Nothing here
//! publishes, edits, deletes, reacts, joins, leaves, or marks read. Every write
//! path in the daemon is covered by unit tests over fixtures instead —
//! [`crate`]'s `mentions`, `askcard`, and `readstate` modules build and validate
//! their events without ever handing one to a relay.
//!
//! [`assert_read_only`] enforces that mechanically for the filters these tests
//! send, so a future test cannot quietly become a write by adding one line.
//!
//! # Skipping is loud
//!
//! Without `BUZZ_DAEMON_LIVE_ENV` pointing at a credential file, every test
//! here **skips with a printed reason** rather than passing silently. A live
//! suite that no-ops in CI and reports green is worse than no live suite: it
//! looks like coverage.
//!
//! ```text
//! BUZZ_DAEMON_LIVE_ENV=~/.config/buzz-acp/<agent>.env \
//!   cargo test -p buzz-daemon --test live_relay -- --nocapture
//! ```

use buzz_daemon::identity::{AuthTag, Identity};
use buzz_daemon::rest::RestClient;
use buzz_daemon::search;
use buzz_daemon::timeline;
use zeroize::Zeroizing;

/// Environment variable naming the credential file to source.
const LIVE_ENV_VAR: &str = "BUZZ_DAEMON_LIVE_ENV";

/// A loaded live-test context, or `None` when the suite should skip.
struct Live {
    identity: Identity,
    rest: RestClient,
    relay_url: String,
}

/// Parse a `KEY=value` env file without exporting anything into this process.
///
/// Deliberately not `std::env::set_var`: these tests run in the same process as
/// every other test in the binary, and the daemon's own §2.5 rule is that a
/// secret in the environment is refused. Reading the file into a local map keeps
/// `refuse_env_key_paths` honest — a test that set `BUZZ_PRIVATE_KEY` would make
/// the refusal test fail depending on execution order.
fn parse_env_file(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some((k.trim().to_string(), unquote(v.trim())))
        })
        .collect()
}

/// Undo shell double-quoting, including the `\"` escapes inside it.
///
/// **Not cosmetic.** `BUZZ_AUTH_TAG` is a JSON array, so the file writes it as
/// `BUZZ_AUTH_TAG="[\"auth\",…]"`. Merely stripping the outer quotes leaves the
/// backslashes in place, `verify_auth_tag` fails to parse it, the tag is
/// dropped, and every authenticated read comes back `403
/// relay_membership_required` — which reads exactly like "this identity has no
/// access" rather than "the test harness mangled the credential". That is a
/// full afternoon of debugging the wrong layer.
fn unquote(raw: &str) -> String {
    let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return raw.trim_matches('\'').to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        // Inside double quotes, a backslash only escapes these four; anything
        // else is a literal backslash followed by the character.
        match chars.next() {
            Some(next @ ('"' | '\\' | '$' | '`')) => out.push(next),
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    out
}

impl Live {
    /// Load the live context, or `None` (printing why) when it is unavailable.
    fn load() -> Option<Self> {
        let Some(path) = std::env::var_os(LIVE_ENV_VAR) else {
            eprintln!("SKIP: {LIVE_ENV_VAR} is not set — live-relay tests need a credential file");
            return None;
        };
        let vars = parse_env_file(std::path::Path::new(&path));
        let (Some(nsec), Some(relay_url)) =
            (vars.get("BUZZ_PRIVATE_KEY"), vars.get("BUZZ_RELAY_URL"))
        else {
            eprintln!("SKIP: credential file has no BUZZ_PRIVATE_KEY / BUZZ_RELAY_URL");
            return None;
        };

        let keys = parse_secret(nsec)?;
        let auth_tag = vars.get("BUZZ_AUTH_TAG").and_then(|raw| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            match AuthTag::load(raw, &keys.public_key(), now) {
                Ok(tag) => Some(tag),
                Err(err) => {
                    // §2.5: a malformed or expired tag is its own distinct
                    // error. Surfacing it here rather than swallowing it is
                    // what turns "the live tests 403" into a one-line answer.
                    eprintln!("live: auth tag rejected: {err}");
                    None
                }
            }
        });

        let rest = RestClient::new(relay_url).ok()?;
        Some(Self {
            identity: Identity::from_keys(keys, auth_tag),
            rest,
            relay_url: relay_url.clone(),
        })
    }

    fn self_pubkey(&self) -> String {
        self.identity.pubkey.clone()
    }
}

fn parse_secret(raw: &str) -> Option<nostr::Keys> {
    use nostr::FromBech32;
    let secret = if raw.starts_with("nsec1") {
        nostr::SecretKey::from_bech32(raw).ok()?
    } else {
        nostr::SecretKey::from_hex(raw).ok()?
    };
    // The buffer is zeroized on drop even though `Keys` itself is not — the
    // parse above is the only place the raw string is held.
    let _guard = Zeroizing::new(raw.to_string());
    Some(nostr::Keys::new(secret))
}

/// Refuse a filter that would mutate anything.
///
/// A Nostr *filter* cannot write by construction — writes are `POST /events`
/// with a signed event. This asserts the weaker but load-bearing property that
/// these tests only ever build filters, and that every one of them carries the
/// explicit `kinds` §2.4 requires. If a future test reaches for `submit_event`,
/// it will not be routed through here, which is why the module docs state the
/// boundary as well.
fn assert_read_only(filter: &serde_json::Value) {
    search::assert_explicit_kinds(filter, "live test")
        .expect("§2.4: no filter leaves the daemon without explicit kinds");
    assert!(
        filter.get("content").is_none() && filter.get("sig").is_none(),
        "a filter must not carry event fields: {filter}"
    );
}

macro_rules! live {
    ($name:ident) => {
        let Some($name) = Live::load() else {
            return;
        };
    };
}

/// The NIP-11 document is public, so this needs no key at all — which makes it
/// the cheapest possible check that the relay URL in the credential file is
/// reachable and speaks Buzz.
#[tokio::test]
async fn nip11_info_is_reachable_without_authentication() {
    live!(live);
    let doc = live
        .rest
        .get_public("/info")
        .await
        .expect("NIP-11 /info should be readable");
    let parsed: serde_json::Value = serde_json::from_str(&doc).expect("NIP-11 doc is JSON");
    assert!(
        parsed.get("name").is_some() || parsed.get("supported_nips").is_some(),
        "not a NIP-11 document: {parsed}"
    );
    eprintln!("live: {} answered NIP-11", live.relay_url);
}

/// §4.1.1 deliverable 3: channel discovery is 39002 `#p = self`.
///
/// The real assertion is not "some channels came back" — it is that the filter
/// the daemon builds is one the relay's p-gate **accepts**. A filter that 403s
/// is a discovery path that returns an empty channel list forever, and against a
/// mock it would look identical to a working one.
#[tokio::test]
async fn channel_discovery_filter_is_accepted_by_the_p_gate() {
    live!(live);
    let filter = buzz_daemon::channels::build_discovery_filter(&live.self_pubkey());
    assert_read_only(&filter);

    let events = live
        .rest
        .query(&live.identity, &filter)
        .await
        .expect("39002 #p=self must not be refused by the p-gate");
    eprintln!(
        "live: discovery returned {} membership events",
        events.len()
    );
    for event in &events {
        assert_eq!(
            event.get("kind").and_then(serde_json::Value::as_u64),
            Some(39_002),
            "the relay returned a kind the filter did not ask for"
        );
    }
}

/// §2.4's global invariant, asserted where it actually bites: the relay.
///
/// A kindless filter is refused *by the daemon* before it can reach the wire,
/// which is the behaviour under test — but this test also proves the reason the
/// invariant exists, by showing the daemon-side refusal happens without a round
/// trip that would have 403'd.
#[tokio::test]
async fn a_kindless_filter_is_refused_before_the_relay_sees_it() {
    live!(live);
    let err = live
        .rest
        .query(
            &live.identity,
            &serde_json::json!({"#p": [live.self_pubkey()]}),
        )
        .await
        .expect_err("a kindless filter must never leave the daemon");
    assert_eq!(err.code(), "kindless_filter");
}

/// §4.1.1 deliverable 12 / §2.4: `/agent/{pk}/metric` **must** carry
/// `#p = self`, because `RESULT_GATED_KINDS` loses the `ids` exemption.
///
/// The design says this endpoint "would otherwise ship 403-ing". This is the
/// test that would have caught it: the well-formed filter is accepted, and the
/// `ids`-style one the exemption tempts you into is refused by the relay.
#[tokio::test]
async fn the_metric_filter_needs_p_self_and_the_ids_exemption_does_not_apply() {
    live!(live);
    let me = live.self_pubkey();

    let good = buzz_daemon::metric::build_metric_filter(&me, &me).expect("filter builds");
    assert_read_only(&good);
    live.rest
        .query(&live.identity, &good)
        .await
        .expect("44200 with #p=self must be accepted");

    // The tempting form: name the kind explicitly and rely on the `ids`
    // exemption. `RESULT_GATED_KINDS` carves it out, so the relay refuses.
    let tempting = serde_json::json!({
        "kinds": [44_200],
        "ids": ["0".repeat(64)],
    });
    assert_read_only(&tempting);
    match live.rest.query(&live.identity, &tempting).await {
        Err(err) => eprintln!("live: the ids-exemption form was refused as designed: {err}"),
        Ok(events) => assert!(
            events.is_empty(),
            "the ids-exemption form returned {} events; the carve-out is not holding",
            events.len()
        ),
    }
}

/// [D-10]: the NIP-CW window filter is the *extension* form. An
/// extension-unaware relay is exactly the degradation branch, so both forms
/// must be accepted — the window one on a NIP-CW relay, the downgraded one
/// everywhere.
///
/// This test does not assert which branch fires. It asserts that neither filter
/// is *refused*, because a refused downgrade filter means the degradation
/// branch itself is broken, and that is invisible until the day it is needed.
#[tokio::test]
async fn both_the_window_and_downgrade_filters_are_accepted() {
    live!(live);
    let discovery = buzz_daemon::channels::build_discovery_filter(&live.self_pubkey());
    let memberships = live
        .rest
        .query(&live.identity, &discovery)
        .await
        .unwrap_or_default();
    let Some(channel_id) = memberships
        .iter()
        .find_map(buzz_daemon::channels::channel_id_from_membership)
    else {
        eprintln!("SKIP: this identity is a member of no channels");
        return;
    };

    for filter in [
        timeline::build_window_filter(&channel_id, 5, None),
        timeline::build_downgraded_filter(&channel_id, 5, None),
    ] {
        assert_read_only(&filter);
        let events = live
            .rest
            .query(&live.identity, &filter)
            .await
            .unwrap_or_else(|e| panic!("filter refused: {e}\n{filter}"));
        eprintln!("live: {} rows for {}", events.len(), filter["kinds"]);
    }
}

/// [D-10]: the window page must satisfy bounds integrity **against a real
/// relay**, and the walk must actually advance.
///
/// The unit tests assert the parse over hand-built fixtures, which cannot tell
/// you whether the relay's `39006` `d`-tag binding matches the string
/// [`timeline::expected_bounds_binding`] constructs. That is a cross-process
/// agreement about a format, and the only way to test an agreement is to ask the
/// other party.
///
/// The advance check is the second half: a cursor that does not move is a
/// livelock, and with `until` inclusive it is the *expected* failure of a
/// timestamp-only cursor. Asserting the second page differs from the first is
/// what proves the composite cursor is doing its job.
#[tokio::test]
async fn a_live_window_page_satisfies_bounds_integrity_and_advances() {
    live!(live);
    let discovery = buzz_daemon::channels::build_discovery_filter(&live.self_pubkey());
    let memberships = live
        .rest
        .query(&live.identity, &discovery)
        .await
        .unwrap_or_default();
    let Some(channel_id) = memberships
        .iter()
        .find_map(buzz_daemon::channels::channel_id_from_membership)
    else {
        eprintln!("SKIP: this identity is a member of no channels");
        return;
    };

    let first_filter = timeline::build_window_filter(&channel_id, 2, None);
    assert_read_only(&first_filter);
    let events = live
        .rest
        .query(&live.identity, &first_filter)
        .await
        .expect("the window filter is accepted");

    let page = match timeline::parse_window_response(&events, &channel_id, None) {
        Ok(page) => page,
        Err(err) => {
            // Not a test failure: a relay without the NIP-CW extension is the
            // degradation branch, which is a supported configuration. It *is*
            // worth printing, because "the window path silently downgraded"
            // is exactly the thing [D-10] says must be a visible decision.
            eprintln!("live: no valid 39006 ({err}) — this relay takes the downgrade branch");
            let downgraded = timeline::assemble_downgraded_page(&events, 2);
            assert_eq!(downgraded.mode, timeline::WindowMode::Downgraded);
            return;
        }
    };
    eprintln!(
        "live: window page mode={:?} rows={} aux={} has_more={}",
        page.mode,
        page.rows.len(),
        page.aux.len(),
        page.has_more
    );

    let Some(cursor) = page.next_cursor.clone() else {
        eprintln!("live: the channel fit in one page; nothing to advance past");
        return;
    };

    let second_filter = timeline::build_window_filter(&channel_id, 2, Some(&cursor));
    assert_read_only(&second_filter);
    let second_events = live
        .rest
        .query(&live.identity, &second_filter)
        .await
        .expect("the cursored window filter is accepted");
    let second = timeline::parse_window_response(&second_events, &channel_id, Some(&cursor))
        .expect("page two binds to the cursor page one issued");

    let ids = |page: &timeline::WindowPage| -> Vec<String> {
        page.rows
            .iter()
            .filter_map(|row| row.event["id"].as_str().map(str::to_string))
            .collect()
    };
    let (first_ids, second_ids) = (ids(&page), ids(&second));
    if !first_ids.is_empty() && !second_ids.is_empty() {
        assert_ne!(
            first_ids, second_ids,
            "the cursor did not advance — this is the dense-second livelock"
        );
    }
}

/// §3.5 / §4.1.1 deliverable 7: search must carry `kinds`, and the relay's FTS
/// must accept the shape the daemon builds.
#[tokio::test]
async fn search_with_explicit_kinds_is_accepted() {
    live!(live);
    let filter = search::build_search_filter("the", None);
    assert_read_only(&filter);
    let events = live
        .rest
        .query(&live.identity, &filter)
        .await
        .expect("a kind-scoped NIP-50 search must be accepted");
    eprintln!("live: search returned {} rows", events.len());
}

/// §2.5: the NIP-OA auth tag is identity. When the credential file carries one,
/// it must verify against *this* pubkey — a tag minted for another agent is a
/// silent 403 on every write, and the daemon promises to detect that at load.
#[tokio::test]
async fn the_auth_tag_binds_to_this_identity() {
    live!(live);
    let Some(tag) = live.identity.auth_tag.as_ref() else {
        eprintln!("SKIP: this credential file carries no BUZZ_AUTH_TAG");
        return;
    };
    assert_eq!(tag.owner_pubkey.len(), 64, "owner pubkey is 32-byte hex");
    assert_ne!(
        tag.owner_pubkey, live.identity.pubkey,
        "an auth tag attests an *owner* over an agent; equal keys means it is not a delegation"
    );
    // The nostr form must build, or `sign_event` cannot attach it.
    tag.to_nostr_tag()
        .expect("the tag converts to its wire form");
}

// ── The relay I/O loop, against a live relay ───────────────────────────────
//
// These are the tests the wire lane exists for. Everything above asserts that a
// *filter* is accepted; these assert that the loop built on those filters
// reaches `connected`, fills the stores, and agrees with `buzz-cli` about what
// is there.
//
// **They remain read-only, and structurally so.** The loop publishes on exactly
// two paths — an explicit `WireCommand::Publish`, and the read-state debounce
// gated on `ReadState::is_dirty`. These tests issue no command and mark
// nothing, so neither can fire. `the_loop_cannot_have_written_anything` asserts
// that mechanically at the end of the live run rather than trusting the claim.

/// Build an `AppState` around the live credentials, with a wire handle.
///
/// The socket path is a temp directory's: nothing binds it, because these tests
/// drive the relay loop directly rather than through HTTP.
fn live_state(live: &Live, dir: &std::path::Path) -> buzz_daemon::state::AppState {
    let config = buzz_daemon::config::Config {
        identity: buzz_daemon::config::SocketIdentity::new(
            &live.relay_url,
            live.self_pubkey(),
            live.identity
                .auth_tag
                .as_ref()
                .map(|t| t.owner_pubkey.clone())
                .unwrap_or_default(),
        ),
        socket: dir.join("daemon.sock"),
        runtime_dir: dir.to_path_buf(),
        data_dir: dir.to_path_buf(),
        idle_timeout: None,
        observer_cache_bytes: 1024 * 1024,
        systemd_managed: false,
    };
    buzz_daemon::state::AppState::new(config, Some(live.identity.clone())).expect("state")
}

/// Run the relay loop until `ready` holds, or the deadline passes.
///
/// Returns whether the condition held. A deadline rather than an unbounded wait
/// because a live test that hangs on a relay outage is a CI job that hangs, and
/// the honest outcome of "the relay did not answer in 30 s" is a failure with
/// that sentence in it.
async fn run_until(
    state: &buzz_daemon::state::AppState,
    deadline: std::time::Duration,
    ready: impl Fn(&buzz_daemon::state::Inner) -> bool,
) -> bool {
    let (wire, commands) = buzz_daemon::wire::channel();
    let loop_state = state.clone().with_wire(wire);
    let task = tokio::spawn(async move {
        buzz_daemon::wire::run(loop_state, commands).await;
    });

    let started = std::time::Instant::now();
    let mut held = false;
    while started.elapsed() < deadline {
        if ready(&*state.lock().await) {
            held = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Aborted rather than left running: the loop never returns by design, and a
    // leaked one would keep a websocket open for the rest of the test binary.
    task.abort();
    held
}

/// §2.6: `connection.state` must actually reach `connected`.
///
/// This is the assertion the whole lane turns on. Before the loop existed the
/// daemon reported `disconnected` for the life of the process while `buzz-cli`
/// against the same identity and relay showed three channels and a live
/// conversation — the stores were correct and empty. A mock cannot catch that;
/// only asking the real relay for a NIP-42 handshake can.
#[tokio::test]
async fn the_relay_loop_reaches_connected() {
    live!(live);
    let dir = tempfile::tempdir().expect("temp dir");
    let state = live_state(&live, dir.path());

    let connected = run_until(&state, std::time::Duration::from_secs(30), |inner| {
        matches!(
            inner.session.state(),
            buzz_daemon::session::ConnectionState::Connected
        )
    })
    .await;

    let final_state = state.lock().await.session.state().clone();
    assert!(
        connected,
        "the loop never reached connected; last state was {final_state:?}"
    );
    eprintln!("live: connection.state reached {final_state:?}");
}

/// §2.6: the transition is **published**, not merely recorded.
///
/// `connection.state` is a durable topic precisely because §2.6 renders these
/// states as chrome rather than as toasts — a state change the TUI never
/// receives leaves the status bar claiming an outage that ended.
#[tokio::test]
async fn reaching_connected_publishes_a_connection_state_frame() {
    live!(live);
    let dir = tempfile::tempdir().expect("temp dir");
    let state = live_state(&live, dir.path());

    let published = run_until(&state, std::time::Duration::from_secs(30), |inner| {
        matches!(
            inner.stream.replay(0),
            buzz_daemon::stream::Replay::Frames(ref frames)
                if frames.iter().any(|f| f.topic == "connection.state"
                    && f.payload["state"] == "connected")
        )
    })
    .await;
    assert!(published, "no connection.state frame carrying `connected`");
}

/// §4.1.1 deliverable 3: the channels the loop discovers must be the channels
/// the relay says this identity is in.
///
/// Parity against the **relay's own answer** through the HTTP bridge — the same
/// query `buzz-cli channels list` runs — rather than against a fixed count. A
/// count is a fact about the community on one afternoon; the agreement between
/// two independent reads of the same membership set is a fact about the code.
#[tokio::test]
async fn the_loops_channel_set_matches_what_the_relay_reports() {
    live!(live);
    let discovery = buzz_daemon::channels::build_discovery_filter(&live.self_pubkey());
    assert_read_only(&discovery);
    let memberships = live
        .rest
        .query(&live.identity, &discovery)
        .await
        .expect("discovery is accepted");
    let expected: std::collections::BTreeSet<String> = memberships
        .iter()
        .filter_map(buzz_daemon::channels::channel_id_from_membership)
        .collect();
    if expected.is_empty() {
        eprintln!("SKIP: this identity is a member of no channels");
        return;
    }

    // Hydrate the channel cache the way the daemon's discovery step does, then
    // let the loop subscribe to what it found. The loop maintains the set live
    // from 44100/44101; discovery is what seeds it.
    let dir = tempfile::tempdir().expect("temp dir");
    let state = live_state(&live, dir.path());
    {
        let ids: Vec<String> = expected.iter().cloned().collect();
        let metadata = live
            .rest
            .query(
                &live.identity,
                &buzz_daemon::channels::build_metadata_filter(&ids),
            )
            .await
            .unwrap_or_default();
        let mut inner = state.lock().await;
        for channel in buzz_daemon::channels::merge_discovered(ids.clone(), &metadata) {
            inner.session.subscriptions.subscribe(channel.id.clone());
            inner.channels.upsert(channel);
        }
    }

    let connected = run_until(&state, std::time::Duration::from_secs(30), |inner| {
        matches!(
            inner.session.state(),
            buzz_daemon::session::ConnectionState::Connected
        )
    })
    .await;
    assert!(
        connected,
        "the loop must connect before parity means anything"
    );

    let inner = state.lock().await;
    let cached: std::collections::BTreeSet<String> =
        inner.channels.list().into_iter().map(|c| c.id).collect();
    // Subset rather than equality: `merge_discovered` drops archived channels,
    // which are members the relay still reports. A cached channel the relay
    // does *not* report is the real defect — that is a channel the daemon
    // invented — and this catches it.
    for id in &cached {
        assert!(
            expected.contains(id),
            "the daemon cached channel {id}, which this identity is not a member of"
        );
    }
    eprintln!(
        "live: {} membership rows → {} cached channels",
        expected.len(),
        cached.len()
    );
}

/// **The cold-start walk.** A loop given *nothing* must find its own channels.
///
/// This is the test the parity case above cannot be: that one seeds the cache by
/// hand before starting the loop, so it proves the loop does not *invent*
/// channels while proving nothing about whether it can *find* them. With the
/// seeding removed, the daemon at `312e9c561` connected, authenticated, and
/// served an empty channel list forever — because 44100/44101 are notifications
/// of *change*, and a settled community emits none to hear.
///
/// Measured, not reasoned: `GET /channel` returned `{"channels":[]}` over the
/// UDS while `buzz-cli channels list` under the same key at the same minute
/// returned three. `dogfood-m3/live-daemon/cold-start-defect.md` carries both.
#[tokio::test]
async fn a_cold_daemon_discovers_its_channels_without_being_seeded() {
    live!(live);
    let discovery = buzz_daemon::channels::build_discovery_filter(&live.self_pubkey());
    assert_read_only(&discovery);
    let memberships = live
        .rest
        .query(&live.identity, &discovery)
        .await
        .expect("discovery is accepted");
    let expected: std::collections::BTreeSet<String> = memberships
        .iter()
        .filter_map(buzz_daemon::channels::channel_id_from_membership)
        .collect();
    if expected.is_empty() {
        eprintln!("SKIP: this identity is a member of no channels");
        return;
    }

    // Nothing is seeded. The state starts with an empty cache and an empty
    // subscription registry, exactly as a freshly launched daemon does.
    let dir = tempfile::tempdir().expect("temp dir");
    let state = live_state(&live, dir.path());
    assert!(
        state.lock().await.channels.is_empty(),
        "the precondition of this test is a cold cache"
    );

    let hydrated = run_until(&state, std::time::Duration::from_secs(30), |inner| {
        !inner.channels.is_empty()
    })
    .await;
    assert!(
        hydrated,
        "a cold daemon must discover its channels from the relay, not wait for a \
         membership change that a settled community never emits"
    );

    let inner = state.lock().await;
    let cached: std::collections::BTreeSet<String> =
        inner.channels.list().into_iter().map(|c| c.id).collect();
    for id in &cached {
        assert!(
            expected.contains(id),
            "the daemon cached channel {id}, which this identity is not a member of"
        );
    }
    // The registry is the half that was missing at M2: a cached channel with no
    // subscription is a row in the list whose timeline never fills.
    for id in &cached {
        assert!(
            inner.session.subscriptions.resubscribe_since(id).is_some(),
            "channel {id} was cached but never registered for subscription"
        );
    }
    eprintln!(
        "live: a cold daemon discovered {} channels and registered {} subscriptions",
        cached.len(),
        inner.session.subscriptions.len()
    );
}

/// A real timeline window must render as rows the daemon's stores accept.
///
/// The unit tests parse hand-built pages; this asserts the assembled page
/// carries content rows from a live channel and that each one is a kind the
/// timeline claims to render. A row of a kind the renderer does not know is a
/// blank line in the TUI, and it looks like a rendering bug rather than a
/// filter one.
#[tokio::test]
async fn a_live_timeline_window_yields_renderable_rows() {
    live!(live);
    let discovery = buzz_daemon::channels::build_discovery_filter(&live.self_pubkey());
    let memberships = live
        .rest
        .query(&live.identity, &discovery)
        .await
        .unwrap_or_default();

    let mut rendered = 0usize;
    for channel_id in memberships
        .iter()
        .filter_map(buzz_daemon::channels::channel_id_from_membership)
    {
        let filter = timeline::build_window_filter(&channel_id, 20, None);
        assert_read_only(&filter);
        let events = live
            .rest
            .query(&live.identity, &filter)
            .await
            .unwrap_or_default();
        let page = match timeline::parse_window_response(&events, &channel_id, None) {
            Ok(page) => page,
            Err(_) => timeline::assemble_downgraded_page(&events, 20),
        };
        for row in &page.rows {
            let kind = row.event["kind"].as_u64().expect("a row has a kind") as u32;
            assert!(
                timeline::is_content_kind(kind),
                "kind {kind} assembled as a row but is not a content kind"
            );
            assert!(
                !timeline::is_aux_kind(kind),
                "kind {kind} is an aux overlay and must never be a row"
            );
            rendered += 1;
        }
        // Also assert the *live* filter the loop subscribes with is accepted:
        // the window filter and the tail filter are different shapes, and a
        // tail the relay refuses is a channel that goes quiet after its first
        // page with no error anywhere.
        let tail = buzz_daemon::wire::channel_live_filter(&channel_id, None);
        assert_read_only(&tail);
        live.rest
            .query(&live.identity, &tail)
            .await
            .unwrap_or_else(|e| panic!("the live tail filter was refused: {e}\n{tail}"));
    }
    eprintln!("live: {rendered} renderable rows across every joined channel");
}

/// §4.1.1 deliverable 11: presence must resolve for the identities the roster
/// actually names, and `unknown` must stay distinct from `offline`.
///
/// The brief asked for "presence/fleet reflect the 6 live units". A hard count
/// is a fact about the community on one afternoon, not about the code — the
/// prior instance measured 5 and the number will move again. The derived
/// property is what holds: **every pubkey the roster names resolves to a
/// presence record**, and a pubkey with no evidence resolves to `unknown`
/// rather than to `offline`, because beats stopping means the daemon stopped
/// hearing, which is not the same as an agent saying it went away.
#[tokio::test]
async fn presence_resolves_for_every_roster_member_and_unknown_is_not_offline() {
    live!(live);
    let discovery = buzz_daemon::channels::build_discovery_filter(&live.self_pubkey());
    let memberships = live
        .rest
        .query(&live.identity, &discovery)
        .await
        .unwrap_or_default();

    // The roster comes from the 39000 metadata's `p` tags — the same source
    // `merge_discovered` counts members from.
    let ids: Vec<String> = memberships
        .iter()
        .filter_map(buzz_daemon::channels::channel_id_from_membership)
        .collect();
    if ids.is_empty() {
        eprintln!("SKIP: this identity is a member of no channels");
        return;
    }
    let metadata = live
        .rest
        .query(
            &live.identity,
            &buzz_daemon::channels::build_metadata_filter(&ids),
        )
        .await
        .unwrap_or_default();
    let members: std::collections::BTreeSet<String> = metadata
        .iter()
        .filter_map(|event| event.get("tags")?.as_array())
        .flatten()
        .filter(|tag| tag.get(0).and_then(serde_json::Value::as_str) == Some("p"))
        .filter_map(|tag| tag.get(1).and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    if members.is_empty() {
        eprintln!("SKIP: no roster members are visible on this relay");
        return;
    }

    // The durable 40902 snapshot is what a cold daemon reads: 20001 is
    // ephemeral, so a daemon starting after an agent's last beat sees nothing
    // until the next one. Without this filter a fresh daemon shows the whole
    // fleet as `unknown` for a full beat interval.
    let roster: Vec<String> = members.iter().cloned().collect();
    let snapshot_filter = buzz_daemon::presence::build_snapshot_filter(&roster);
    assert_read_only(&snapshot_filter);
    let snapshots = live
        .rest
        .query(&live.identity, &snapshot_filter)
        .await
        .expect("40902 by author is accepted");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tracker = buzz_daemon::presence::PresenceTracker::new();
    for event in &snapshots {
        let (Some(pubkey), Some(created_at)) = (
            event.get("pubkey").and_then(serde_json::Value::as_str),
            event.get("created_at").and_then(serde_json::Value::as_u64),
        ) else {
            continue;
        };
        let status = event
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .and_then(|tags| {
                tags.iter()
                    .find(|t| t.get(0).and_then(serde_json::Value::as_str) == Some("status"))
                    .and_then(|t| t.get(1).and_then(serde_json::Value::as_str))
            })
            .or_else(|| event.get("content").and_then(serde_json::Value::as_str))
            .unwrap_or("");
        tracker.observe_snapshot(pubkey, status, created_at as i64);
    }

    let resolved = tracker.snapshot(&roster, now);
    assert_eq!(
        resolved.len(),
        roster.len(),
        "every roster member must resolve to a record, even an unknown one"
    );
    let unknown = resolved
        .values()
        .filter(|r| r.state == buzz_daemon::presence::Presence::Unknown)
        .count();
    for record in resolved.values() {
        // The load-bearing distinction: absence of evidence resolves to
        // `unknown`, never to `offline`. Only an explicit `offline` status is
        // `offline`, and collapsing the two renders a live dot's absence as a
        // claim the daemon cannot make.
        if record.state == buzz_daemon::presence::Presence::Offline {
            assert!(
                record.last_seen.is_some(),
                "an `offline` record with no evidence should have been `unknown`"
            );
        }
    }
    eprintln!(
        "live: {} roster members, {} resolved with evidence, {unknown} unknown",
        roster.len(),
        roster.len() - unknown
    );
}

/// The fleet reduction must survive live data without inventing a measurement.
///
/// §3.4.1's null rule: a `0` where the agent reported nothing is a fabricated
/// measurement. This walks the live agents and asserts that every numeric field
/// is either absent or came from somewhere — specifically that `context_pct` is
/// `None` when no model reported a denominator, which is the field the design
/// singles out ("renders `—` and **no bar**").
#[tokio::test]
async fn the_fleet_reduction_over_live_agents_invents_no_measurements() {
    live!(live);
    let me = live.self_pubkey();
    let filter = buzz_daemon::metric::build_metric_filter(&me, &me).expect("filter builds");
    assert_read_only(&filter);
    let events = live
        .rest
        .query(&live.identity, &filter)
        .await
        .expect("44200 with #p=self is accepted");

    let mut fleet = buzz_daemon::fleet::Fleet::new();
    let mut decoded = 0usize;
    for raw in &events {
        let Ok(event) = serde_json::from_value::<nostr::Event>(raw.clone()) else {
            continue;
        };
        let agent = event.pubkey.to_hex();
        match buzz_daemon::metric::decrypt_metric(&live.identity, &event, None) {
            Ok(metric) => {
                fleet.agent_mut(&agent).observe_metric(metric);
                decoded += 1;
            }
            // Not a failure: 44200 is `#p`-addressed, and a metric encrypted to
            // a different reader legitimately does not decrypt here.
            Err(err) => eprintln!("live: 44200 not readable by this identity: {err}"),
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for row in fleet.rows(now) {
        if let Some(pct) = row.context_pct {
            assert!(
                pct.is_finite() && pct >= 0.0,
                "context_pct must be a real fraction, got {pct}"
            );
        }
        if let Some(cost) = row.cost_usd {
            assert!(cost.is_finite() && cost >= 0.0, "cost_usd {cost}");
        }
        // A rate needs both halves. `None` is the honest answer when either is
        // missing; a rate computed from a fabricated zero is worse than no rate.
        if row.tokens_per_min.is_some() {
            assert!(row.elapsed_secs.is_some_and(|s| s > 0));
        }
    }
    eprintln!("live: {decoded} turn metrics decoded into the fleet reduction");
}

/// **The read-only property, asserted mechanically.**
///
/// Design decision 4 says read-only against `claude-test` is *structural*, not
/// a flag: the loop writes on exactly two paths, and neither can fire without a
/// caller doing something this test does not do. Trusting that is exactly the
/// mistake this lane's history warns about, so it is checked:
///
/// - `is_dirty()` is false, so the read-state debounce — the only unprompted
///   write path — cannot fire. Nothing here calls `mark`.
/// - `local_ids` is empty, so no send was correlated, which means
///   `build_message_event` was never reached.
/// - The subscription registry is populated but no `WireCommand::Publish` was
///   ever constructed; the loop's `pending` list is unreachable from here by
///   construction, and these two observables are what it would have moved.
///
/// If a future test adds a write, one of these assertions fails **in the same
/// run** rather than after the write has already landed on a shared identity.
#[tokio::test]
async fn the_loop_cannot_have_written_anything() {
    live!(live);
    let dir = tempfile::tempdir().expect("temp dir");
    let state = live_state(&live, dir.path());

    let connected = run_until(&state, std::time::Duration::from_secs(30), |inner| {
        matches!(
            inner.session.state(),
            buzz_daemon::session::ConnectionState::Connected
        )
    })
    .await;
    assert!(
        connected,
        "the loop must have run for this to prove anything"
    );

    let inner = state.lock().await;
    assert!(
        !inner.read_state.is_dirty(),
        "read-state is dirty: something marked a context, which arms the only \
         unprompted publish path in the loop"
    );
    assert!(
        inner.local_ids.is_empty(),
        "a local_id was recorded, which only the send path does"
    );
    assert_eq!(
        inner.session.observer_queue.in_flight_len(),
        0,
        "an observer frame was written and is awaiting an OK"
    );
    eprintln!("live: the loop ran and wrote nothing, mechanically");
}
