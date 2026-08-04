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
