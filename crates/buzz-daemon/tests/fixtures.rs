//! Wire-truth fixtures — the relay side of the fixture protocol (`DESIGN.md` §5.4).
//!
//! # Two halves of one protocol
//!
//! §5.4's scenarios are **client-facing**: `{atMs, kind, payload}` replayed into
//! the TUI's fake transport, in the daemon's own snake_case API vocabulary. That
//! half lives in `tui/fixtures/*.jsonl` and the TUI lane owns it.
//!
//! This file owns the other half: `tui/fixtures/wire/*.json`, the **relay-shaped
//! events** the daemon actually consumes. Nostr events with real signatures and
//! real NIP-44 ciphertext, plus the filters the daemon emits for them.
//!
//! The split matters because the two halves fail differently. A client fixture
//! that drifts makes a snapshot test fail loudly. A *wire* assumption that
//! drifts — a tag name, a `d`-tag binding format, a ciphertext envelope — fails
//! only against a real relay, which is exactly the class of bug §5.4's fixtures
//! cannot catch and this file can.
//!
//! # These are generated, not hand-written
//!
//! Every signature and every ciphertext here is produced by the same
//! `buzz_core` and `buzz_sdk` code paths the relay and the harness use. A
//! hand-written fixture would be a second implementation of the wire format,
//! and it would agree with the code that wrote it rather than with the protocol.
//!
//! Regenerate with:
//!
//! ```text
//! BUZZ_DAEMON_BLESS_FIXTURES=1 cargo test -p buzz-daemon --test fixtures
//! ```
//!
//! Without the variable the tests **verify** the committed fixtures parse, so a
//! change to the daemon's parsers that breaks the wire contract fails here
//! rather than in production.

use std::path::{Path, PathBuf};

use buzz_daemon::{askcard, channels, metric, observer, readstate, timeline};
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag};

/// The frozen clock every fixture is authored against, matching
/// `tui/scripts/gen-fixtures.ts`'s `T0` (2026-08-04T14:12:00Z).
///
/// Shared with the client half on purpose: a wire event timestamped outside the
/// window the client fixture renders would be a fixture pair that cannot
/// describe the same moment.
const T0: i64 = 1_785_852_720;

/// Deterministic keys, so a regeneration produces the same pubkeys.
///
/// A fresh `Keys::generate()` per run would make every regeneration a diff of
/// noise, and a reviewer who cannot read the diff cannot review the fixture.
fn keys_from_seed(seed: u8) -> Keys {
    let secret = nostr::SecretKey::from_slice(&[seed; 32]).expect("a valid secret");
    Keys::new(secret)
}

fn owner() -> Keys {
    keys_from_seed(1)
}

fn agent() -> Keys {
    keys_from_seed(2)
}

fn human() -> Keys {
    keys_from_seed(3)
}

const CHANNEL: &str = "11111111-1111-1111-1111-111111111111";

fn wire_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tui/fixtures/wire")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tui/fixtures/wire"))
}

/// Write a fixture when blessing, or assert the committed one still agrees.
///
/// # Why this is not a byte comparison
///
/// Two of the fields in every event here are **deliberately non-deterministic**,
/// and a fixture harness that ignores that never passes:
///
/// - **BIP-340 signatures carry auxiliary randomness.** Signing the same event
///   twice with the same key produces two different valid `sig` values. That is
///   the scheme working as designed, not drift.
/// - **NIP-44 ciphertext carries a random nonce.** Encrypting the same payload
///   twice produces two different valid ciphertexts. Again by design — a
///   deterministic nonce is a NIP-44 vulnerability, not a testing convenience.
///
/// So a fixture that pinned those bytes would pin *randomness* and fail on
/// every run, which is exactly what the first draft of this file did. Freezing
/// them by seeding the RNG would be worse: it would make the fixture a record
/// of one PRNG's output rather than of the protocol, and it would silently stop
/// exercising the real signing path.
///
/// What the comparison actually asserts is what a fixture is *for*: the
/// **structure** is unchanged (tags, kinds, filters, decoded values — everything
/// the daemon parses), and the committed signatures still **verify**. A tag
/// rename, a filter key change, or a decode regression fails here; a fresh
/// nonce does not.
fn bless_or_verify(name: &str, value: &serde_json::Value) {
    let dir = wire_dir();
    let path = dir.join(format!("{name}.json"));

    if std::env::var_os("BUZZ_DAEMON_BLESS_FIXTURES").is_some() {
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
        std::fs::write(&path, rendered).expect("write fixture");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing fixture {}; regenerate with BUZZ_DAEMON_BLESS_FIXTURES=1",
            path.display()
        )
    });
    let committed: serde_json::Value =
        serde_json::from_str(&committed).expect("committed fixture is JSON");

    assert_eq!(
        redact_nondeterministic(&committed),
        redact_nondeterministic(value),
        "wire fixture {name} drifted in a field the daemon parses; if this is \
         intended, regenerate with BUZZ_DAEMON_BLESS_FIXTURES=1 and review the diff"
    );
    assert_committed_signatures_verify(name, &committed);
}

/// Replace the two non-deterministic fields with markers, everywhere they
/// appear.
///
/// `sig` and `id` travel together: the id is a hash *over* the content, so a
/// fresh nonce changes both. Redacting the id as well is therefore not a
/// weakening — it is the same fact twice. Everything the daemon reads to make a
/// decision (kinds, tags, filters, decoded fields) survives the redaction, which
/// is what keeps the comparison meaningful.
fn redact_nondeterministic(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let is_event = map.contains_key("sig") && map.contains_key("id");
            serde_json::Value::Object(
                map.iter()
                    .map(|(key, child)| {
                        let redacted = match key.as_str() {
                            "sig" | "id" if is_event => serde_json::json!("<nondeterministic>"),
                            // Only an *event's* content is ciphertext; a filter's
                            // or a decoded payload's `content` is structure and
                            // must still be compared.
                            "content" if is_event && looks_like_nip44(child) => {
                                serde_json::json!("<nip44-ciphertext>")
                            }
                            _ => redact_nondeterministic(child),
                        };
                        (key.clone(), redacted)
                    })
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_nondeterministic).collect())
        }
        other => other.clone(),
    }
}

fn looks_like_nip44(value: &serde_json::Value) -> bool {
    value
        .as_str()
        .is_some_and(buzz_core::observer::content_looks_like_nip44)
}

/// Assert every event in a committed fixture still carries a **valid**
/// signature.
///
/// This is the half the redaction gives up, recovered: the bytes need not match
/// what a fresh run produces, but they must still be a real signed event. A
/// fixture whose signature has been hand-edited — or corrupted by a merge —
/// would otherwise sail through as "structure unchanged".
///
/// Not every fixture carries events. `readstate-multidevice` is *decrypted*
/// 30078 blobs, because the thing under test is the max-wins merge and the
/// envelope around it is NIP-44 self-encryption the daemon does not parse
/// differently from anywhere else. Fixtures like that opt out by declaring
/// `"signed_events": false`, which is a statement in the file rather than a
/// silent exemption in this function — a fixture that *should* carry events and
/// does not still fails.
fn assert_committed_signatures_verify(name: &str, value: &serde_json::Value) {
    let expects_events = value
        .get("signed_events")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    let mut checked = 0;
    visit_events(value, &mut |event_json| {
        let event: nostr::Event = serde_json::from_value(event_json.clone())
            .unwrap_or_else(|e| panic!("{name}: committed event is not a nostr::Event: {e}"));
        assert!(
            event.verify_id(),
            "{name}: committed event id does not hash"
        );
        assert!(
            event.verify_signature(),
            "{name}: committed event signature does not verify"
        );
        checked += 1;
    });

    if expects_events {
        assert!(
            checked > 0,
            "{name}: no signed events found — either the fixture lost its events, \
             or it should declare \"signed_events\": false"
        );
    } else {
        assert_eq!(
            checked, 0,
            "{name} declares no signed events but carries {checked}"
        );
    }
}

fn visit_events(value: &serde_json::Value, f: &mut impl FnMut(&serde_json::Value)) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("sig") && map.contains_key("id") && map.contains_key("kind") {
                f(value);
                return;
            }
            for child in map.values() {
                visit_events(child, f);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                visit_events(item, f);
            }
        }
        _ => {}
    }
}

// ── observer-turn: the full 24200 telemetry frame ─────────────────────────

/// `observer-turn` — a real kind-24200 frame: NIP-44 encrypted to the owner,
/// signed by the agent, tagged the way `build_agent_observer_frame` tags one.
///
/// The fixture the whole activity viewer rests on. It is regenerated rather than
/// transcribed because the ciphertext envelope is not something a human can
/// write by hand and check.
#[test]
fn wire_observer_turn() {
    let (owner_keys, agent_keys) = (owner(), agent());
    let payload = serde_json::json!({
        "seq": 41,
        "timestamp": "2026-08-04T14:12:00Z",
        "kind": "acp_tool_call",
        "channelId": CHANNEL,
        "sessionId": "sess-1",
        "turnId": "4a91",
        "startedAt": "2026-08-04T14:02:11Z",
        "payload": {"tool": "bash", "args": "cargo test -p buzz-daemon"},
    });
    let ciphertext = buzz_core::observer::encrypt_observer_payload(
        &agent_keys,
        &owner_keys.public_key(),
        &payload,
    )
    .expect("encrypt");

    let event = EventBuilder::new(
        Kind::Custom(buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16),
        ciphertext,
    )
    .tags([
        Tag::public_key(owner_keys.public_key()),
        Tag::parse([
            buzz_core::observer::OBSERVER_AGENT_TAG,
            &agent_keys.public_key().to_hex(),
        ])
        .unwrap(),
        Tag::parse([
            buzz_core::observer::OBSERVER_FRAME_TAG,
            buzz_core::observer::OBSERVER_FRAME_TELEMETRY,
        ])
        .unwrap(),
    ])
    .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
    .sign_with_keys(&agent_keys)
    .expect("sign");

    let fixture = serde_json::json!({
        "note": "kind:24200 telemetry frame — real signature, real NIP-44 ciphertext",
        "owner_pubkey": owner_keys.public_key().to_hex(),
        "agent_pubkey": agent_keys.public_key().to_hex(),
        "subscription_filter": observer::build_observer_filter(
            &owner_keys.public_key().to_hex(),
            T0,
        ),
        "event": serde_json::from_str::<serde_json::Value>(&event.as_json()).unwrap(),
    });
    bless_or_verify("observer-turn", &fixture);

    // The fixture is only useful if the daemon's own pipeline accepts it — a
    // fixture nothing consumes is a file, not a test.
    let identity = buzz_daemon::identity::Identity::from_keys(owner_keys, None);
    let mut pipeline = observer::ObserverPipeline::new();
    pipeline.register_agent(agent_keys.public_key().to_hex(), &identity, T0);
    let observer::Ingest::Accepted(frame) = pipeline.ingest(&event, &identity, T0) else {
        panic!("the wire fixture must survive all nine guards");
    };
    assert_eq!(frame.seq, 41);
    assert_eq!(frame.kind, "acp_tool_call");
}

/// `observer-replay` — the same frame re-delivered **out of window**.
///
/// §5.4's `observer-replay` scenario, wire side. Guard 0 is the only thing
/// standing between a captured frame and an archive that accepts it forever, so
/// the fixture that proves it exists.
#[test]
fn wire_observer_replay_out_of_window() {
    let (owner_keys, agent_keys) = (owner(), agent());
    let identity = buzz_daemon::identity::Identity::from_keys(owner_keys.clone(), None);
    let payload = serde_json::json!({
        "seq": 41,
        "timestamp": "2026-08-04T14:12:00Z",
        "kind": "acp_tool_call",
        "payload": {},
    });
    let ciphertext = buzz_core::observer::encrypt_observer_payload(
        &agent_keys,
        &owner_keys.public_key(),
        &payload,
    )
    .unwrap();
    let event = EventBuilder::new(
        Kind::Custom(buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16),
        ciphertext,
    )
    .tags([Tag::parse([
        buzz_core::observer::OBSERVER_AGENT_TAG,
        &agent_keys.public_key().to_hex(),
    ])
    .unwrap()])
    .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
    .sign_with_keys(&agent_keys)
    .unwrap();

    let mut pipeline = observer::ObserverPipeline::new();
    pipeline.register_agent(agent_keys.public_key().to_hex(), &identity, T0);

    // In window: accepted once, deduped on the replay.
    assert!(matches!(
        pipeline.ingest(&event, &identity, T0),
        observer::Ingest::Accepted(_)
    ));
    assert_eq!(
        pipeline.ingest(&event, &identity, T0),
        observer::Ingest::Dropped(observer::Guard::Dedup)
    );

    // Out of window: refused by guard 0, which is the guard that still holds
    // after the dedup window has rotated the triple out.
    let replayed_at = T0 + observer::OBSERVER_FRESHNESS_SECS + 1;
    let mut fresh = observer::ObserverPipeline::new();
    fresh.register_agent(agent_keys.public_key().to_hex(), &identity, replayed_at);
    assert_eq!(
        fresh.ingest(&event, &identity, replayed_at),
        observer::Ingest::Dropped(observer::Guard::Freshness),
        "a replay outside the window must not re-enter after dedup eviction"
    );

    bless_or_verify(
        "observer-replay",
        &serde_json::json!({
            "note": "the same frame, in-window (deduped) and out-of-window (guard 0)",
            "freshness_window_secs": observer::OBSERVER_FRESHNESS_SECS,
            "created_at": T0,
            "replayed_at": replayed_at,
            "event": serde_json::from_str::<serde_json::Value>(&event.as_json()).unwrap(),
        }),
    );
}

// ── window: the NIP-CW page ───────────────────────────────────────────────

/// `window-page` — a NIP-CW window response with its `39006` and `39005`
/// overlays, plus the filter that produced it.
///
/// The `d`-tag binding format is a **cross-process agreement** with the relay,
/// so the fixture records the exact string the daemon expects to see echoed.
#[test]
fn wire_window_page() {
    let author = human();
    let mut events: Vec<serde_json::Value> = Vec::new();

    let root = EventBuilder::new(Kind::Custom(9), "read-state slots are the risky part")
        .tags([Tag::parse(["h", CHANNEL]).unwrap()])
        .custom_created_at(nostr::Timestamp::from_secs(T0 as u64 - 600))
        .sign_with_keys(&author)
        .unwrap();
    let root_id = root.id.to_hex();
    events.push(serde_json::from_str(&root.as_json()).unwrap());

    let second = EventBuilder::new(Kind::Custom(9), "shipping the downgrade branch too")
        .tags([Tag::parse(["h", CHANNEL]).unwrap()])
        .custom_created_at(nostr::Timestamp::from_secs(T0 as u64 - 300))
        .sign_with_keys(&author)
        .unwrap();
    events.push(serde_json::from_str(&second.as_json()).unwrap());

    // Relay-signed overlays. Signed by the *relay*, which is what makes them
    // trustworthy as a summary the client did not compute.
    let relay = keys_from_seed(9);
    let summary = EventBuilder::new(
        Kind::Custom(timeline::KIND_THREAD_SUMMARY as u16),
        serde_json::json!({
            "reply_count": 4,
            "descendant_count": 6,
            "last_reply_at": T0 - 120,
            "participants": [author.public_key().to_hex(), agent().public_key().to_hex()],
        })
        .to_string(),
    )
    .tags([Tag::parse(["e", &root_id]).unwrap()])
    .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
    .sign_with_keys(&relay)
    .unwrap();
    events.push(serde_json::from_str(&summary.as_json()).unwrap());

    let binding = timeline::expected_bounds_binding(CHANNEL, None);
    let bounds = EventBuilder::new(
        Kind::Custom(timeline::KIND_WINDOW_BOUNDS as u16),
        serde_json::json!({
            "has_more": true,
            "next_cursor": {"created_at": T0 - 600, "id": root_id},
        })
        .to_string(),
    )
    .tags([Tag::parse(["d", &binding]).unwrap()])
    .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
    .sign_with_keys(&relay)
    .unwrap();
    events.push(serde_json::from_str(&bounds.as_json()).unwrap());

    bless_or_verify(
        "window-page",
        &serde_json::json!({
            "note": "NIP-CW page: content rows + 39005 summary + 39006 bounds",
            "channel_id": CHANNEL,
            "request_filter": timeline::build_window_filter(CHANNEL, 2, None),
            "downgrade_filter": timeline::build_downgraded_filter(CHANNEL, 2, None),
            "expected_bounds_binding": binding,
            "events": events,
        }),
    );

    let page = timeline::parse_window_response(&events, CHANNEL, None).expect("the page parses");
    assert_eq!(page.mode, timeline::WindowMode::Nipcw);
    assert_eq!(page.rows.len(), 2, "overlays are not rows");
    assert_eq!(page.rows[0].thread.as_ref().unwrap().reply_count, 4);
    assert!(page.has_more);
}

// ── channels: discovery ───────────────────────────────────────────────────

/// `channel-discovery` — the 39002 → 39000 pair, and the filters between them.
#[test]
fn wire_channel_discovery() {
    let relay = keys_from_seed(9);
    let me = owner().public_key().to_hex();

    let membership = EventBuilder::new(Kind::Custom(39_002), "")
        .tags([
            Tag::parse(["d", CHANNEL]).unwrap(),
            Tag::parse(["p", &me]).unwrap(),
            Tag::parse(["p", &human().public_key().to_hex()]).unwrap(),
        ])
        .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
        .sign_with_keys(&relay)
        .unwrap();
    let metadata = EventBuilder::new(Kind::Custom(39_000), "")
        .tags([
            Tag::parse(["d", CHANNEL]).unwrap(),
            Tag::parse(["name", "engineering"]).unwrap(),
            Tag::parse(["topic", "relay + desktop · agents welcome"]).unwrap(),
            Tag::parse(["t", "channel"]).unwrap(),
        ])
        .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
        .sign_with_keys(&relay)
        .unwrap();

    let membership_json: serde_json::Value = serde_json::from_str(&membership.as_json()).unwrap();
    let metadata_json: serde_json::Value = serde_json::from_str(&metadata.as_json()).unwrap();

    bless_or_verify(
        "channel-discovery",
        &serde_json::json!({
            "note": "39002 #p=self, then the 39000 batch it discovers",
            "self_pubkey": me,
            "discovery_filter": channels::build_discovery_filter(&me),
            "metadata_filter": channels::build_metadata_filter(&[CHANNEL.to_string()]),
            "membership_filter": channels::build_membership_filter(&me, Some(T0 as u64)),
            "membership_event": membership_json,
            "metadata_event": metadata_json,
        }),
    );

    assert_eq!(
        channels::channel_id_from_membership(&membership_json).as_deref(),
        Some(CHANNEL)
    );
    let merged = channels::merge_discovered(vec![CHANNEL.into()], &[metadata_json]);
    assert_eq!(merged[0].name, "engineering");
}

// ── ask card ──────────────────────────────────────────────────────────────

/// `agent-ask` — the `["ask", json]` tag on a real kind:9, both routings.
///
/// §5.4 names `ask-askowner` and `ask-auto` as separate scenarios because the
/// difference is entirely in what the client is *allowed to do*, and a fixture
/// that only covered the actionable one would let the no-affordance rule ship
/// untested.
#[test]
fn wire_agent_ask() {
    let agent_keys = agent();
    let ask_payload = serde_json::json!({
        "v": 1,
        "question": "Which database for the cache?",
        "options": [
            {"label": "Postgres", "description": "already a dependency"},
            {"label": "SQLite", "description": "no server to run"},
        ],
        "multiSelect": false,
        "allowFreeText": false,
        "index": 0,
        "total": 1,
    });
    let event = EventBuilder::new(
        Kind::Custom(9),
        "Which database for the cache?\n1. Postgres\n2. SQLite",
    )
    .tags([
        Tag::parse(["h", CHANNEL]).unwrap(),
        Tag::parse([askcard::ASK_TAG_NAME, &ask_payload.to_string()]).unwrap(),
    ])
    .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
    .sign_with_keys(&agent_keys)
    .unwrap();

    let tags: Vec<Vec<String>> = event.tags.iter().map(|t| t.as_slice().to_vec()).collect();

    let ask_owner =
        askcard::parse_ask_tag(&tags, askcard::AskRouting::AskOwner).expect("the ask tag parses");
    let auto = askcard::parse_ask_tag(&tags, askcard::AskRouting::Auto).expect("parses");

    bless_or_verify(
        "agent-ask",
        &serde_json::json!({
            "note": "kind:9 with an ask tag; both routings, and the answer's tags",
            "event": serde_json::from_str::<serde_json::Value>(&event.as_json()).unwrap(),
            "parsed_ask_owner": ask_owner,
            "parsed_auto": auto,
            "answer_content_for_option_1": askcard::ask_reply_content(&[0]),
            "answer_tags": askcard::ask_answer_tags(
                CHANNEL,
                &event.id.to_hex(),
                &agent_keys.public_key().to_hex(),
            ),
        }),
    );

    assert!(askcard::is_answerable(&ask_owner));
    assert!(
        !askcard::is_answerable(&auto),
        "§5.5: no key answers an Auto card"
    );
}

// ── NIP-AM ────────────────────────────────────────────────────────────────

/// `agent-usage` — a real encrypted kind-44200, and the `#p = self` filter.
///
/// The filter is in the fixture because §2.4 warns the endpoint "would
/// otherwise ship 403-ing", and the live suite has confirmed the relay refuses
/// the `ids`-exemption form.
#[test]
fn wire_agent_usage() {
    let (owner_keys, agent_keys) = (owner(), agent());
    let payload = buzz_core::agent_turn_metric::AgentTurnMetricPayload {
        harness: "claude-agent-acp".into(),
        model: Some("claude-opus-5".into()),
        channel_id: Some(CHANNEL.into()),
        session_id: Some("sess-1".into()),
        turn_id: Some("4a91".into()),
        turn_seq: Some(3),
        timestamp: "2026-08-04T14:12:00Z".into(),
        turn: Some(buzz_core::agent_turn_metric::TokenCounts {
            input_tokens: Some(12_480),
            output_tokens: Some(1_932),
            total_tokens: None,
            cost_usd: Some(0.41),
            cache_read_tokens: Some(9_120),
            cache_write_tokens: Some(340),
        }),
        cumulative: None,
        delta_reliable: true,
        stop_reason: Some(buzz_core::agent_turn_metric::StopReason::EndTurn),
    };
    let ciphertext = buzz_core::observer::encrypt_observer_payload(
        &agent_keys,
        &owner_keys.public_key(),
        &payload,
    )
    .unwrap();
    let event = EventBuilder::new(
        Kind::Custom(metric::KIND_AGENT_TURN_METRIC as u16),
        ciphertext,
    )
    .tags([Tag::public_key(owner_keys.public_key())])
    .custom_created_at(nostr::Timestamp::from_secs(T0 as u64))
    .sign_with_keys(&agent_keys)
    .unwrap();

    let me = owner_keys.public_key().to_hex();
    let identity = buzz_daemon::identity::Identity::from_keys(owner_keys, None);
    let decoded = metric::decrypt_metric(&identity, &event, Some(200_000)).expect("decodes");

    bless_or_verify(
        "agent-usage",
        &serde_json::json!({
            "note": "kind:44200 NIP-AM metric — the filter MUST carry #p = self",
            "filter": metric::build_metric_filter(&agent_keys.public_key().to_hex(), &me).unwrap(),
            "event": serde_json::from_str::<serde_json::Value>(&event.as_json()).unwrap(),
            "decoded": decoded,
        }),
    );

    assert_eq!(decoded.total_tokens(), Some(14_412));
    assert_eq!(decoded.stop_reason.as_deref(), Some("end_turn"));
}

// ── read state ────────────────────────────────────────────────────────────

/// `readstate-multidevice` — two devices' slots and the merged frontier.
///
/// §5.4's scenario, wire side. The blob shape is what a *second client* writes,
/// so the fixture is the contract that lets the daemon merge with the desktop
/// rather than beside it.
#[test]
fn wire_readstate_multidevice() {
    let thread = format!("thread:{}", "ab".repeat(32));
    let msg = format!("msg:{}", "cd".repeat(32));

    let desktop = readstate::ReadStateBlob {
        v: 1,
        client_id: "desktop-abc".into(),
        contexts: [
            (CHANNEL.to_string(), (T0 - 600) as u64),
            (thread.clone(), (T0 - 300) as u64),
        ]
        .into_iter()
        .collect(),
    };
    let phone = readstate::ReadStateBlob {
        v: 1,
        client_id: "phone-def".into(),
        contexts: [
            // Behind on the channel, ahead on one message.
            (CHANNEL.to_string(), (T0 - 900) as u64),
            (msg.clone(), T0 as u64),
        ]
        .into_iter()
        .collect(),
    };

    let mut state = readstate::ReadState::new("daemon-xyz", "slot-a");
    state.merge(&desktop);
    state.merge(&phone);

    bless_or_verify(
        "readstate-multidevice",
        &serde_json::json!({
            "note": "two clients' 30078 blobs; max-wins merge, never last-write-wins",
            // Decrypted blobs: the thing under test is the merge, and the
            // NIP-44 self-encrypted envelope around it is not parsed any
            // differently here than anywhere else.
            "signed_events": false,
            "d_tag": state.slot_d_tag_for(0),
            "desktop_blob": desktop,
            "phone_blob": phone,
            "merged": {
                "channel": state.own_marker(CHANNEL),
                "thread": state.own_marker(&thread),
                "msg": state.own_marker(&msg),
            },
        }),
    );

    assert_eq!(
        state.own_marker(CHANNEL),
        Some((T0 - 600) as u64),
        "the phone's older channel marker must not rewind the desktop's"
    );
    assert_eq!(state.own_marker(&msg), Some(T0 as u64));
}

/// Every fixture this file writes must be readable by the module that consumes
/// it — the guard against a `wire/` directory that drifts into decoration.
#[test]
fn every_committed_wire_fixture_is_valid_json() {
    let dir = wire_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // Blessing has not run yet; the per-fixture tests will say so with a
        // more useful message than this one could.
        return;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable");
        let value: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            value.get("note").is_some(),
            "{} has no `note` explaining what it is for",
            path.display()
        );
        count += 1;
    }
    assert!(
        count >= 6,
        "expected the full wire fixture set, found {count}"
    );
}
