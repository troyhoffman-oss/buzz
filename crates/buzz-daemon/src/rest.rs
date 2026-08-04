//! The relay's NIP-98 HTTP bridge, as the daemon consumes it.
//!
//! Implements the `RestClient` of `daemon-api.md` §0.3, composed the way
//! `crates/buzz-cli/src/client.rs` composes `BuzzClient` — the design calls that
//! file "the closest thing to a spec for what a non-desktop Buzz client needs",
//! and the daemon is a non-desktop Buzz client.
//!
//! Re-exported as [`crate::session::RestClient`], which is the path the rest of
//! the crate and the design document both name.
//!
//! # What is ported verbatim, and why each one matters
//!
//! - **NIP-98 is re-signed per attempt.** The `nonce` tag is a fresh UUID, which
//!   is what makes a retry safe against the relay's replay guard
//!   (`sign_nip98`, `crates/buzz-cli/src/client.rs:84`). Reusing one signature
//!   across attempts turns a transient 503 into an auth failure.
//! - **`x-auth-tag` rides every bridge call.** `with_auth_tag`
//!   (`client.rs:614`) — under NIP-OA the header is what carries membership
//!   delegation to the *HTTP* surface, and its absence is a 403 that looks like
//!   a permissions bug.
//! - **The pagination cursor is composite.** `advance_query_cursor`
//!   (`client.rs:500`) sets both `until` and `before_id`; a `until`-only cursor
//!   loses or repeats same-second events. [D-6] and [D-10] both rest on this.
//! - **Moderation kinds 9040–9044 are never blindly retried.** They execute at
//!   the relay *before* dedup, so an ambiguous outcome is
//!   [`DaemonError::DeliveryUnknown`], not a resend (§2.7). Wave 1 ships no
//!   moderation endpoint, but the *client* carries the rule so a Wave-3
//!   endpoint cannot be written without it.
//!
//! # What is deliberately different
//!
//! `BuzzClient` returns raw `String` bodies and leaves parsing to each command.
//! The daemon parses once, here, because every caller wants
//! `Vec<serde_json::Value>` and a per-caller `serde_json::from_str` is a
//! per-caller opportunity to get the error path wrong.

use std::time::Duration;

use base64::Engine as _;
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag};
use sha2::{Digest, Sha256};

use crate::error::{DaemonError, Result};
use crate::identity::Identity;

/// Maximum attempts per request: the initial one plus two retries.
///
/// Origin: `crates/buzz-cli/src/client.rs:122` (`RETRY_MAX_ATTEMPTS`).
pub const RETRY_MAX_ATTEMPTS: u32 = 3;

/// Full-jitter ceilings, in seconds, for the delay before attempt `i + 1`.
///
/// Origin: `crates/buzz-cli/src/client.rs:126` (`RETRY_BASE_SECS`).
pub const RETRY_BASE_SECS: [f64; 2] = [0.5, 1.5];

/// Defensive cap on a relay-provided `retry in Ns` hint.
///
/// Origin: `crates/buzz-cli/src/client.rs:130` (`RETRY_IN_MAX_SECS`).
pub const RETRY_IN_MAX_SECS: u64 = 30;

/// Events per bridge page. The bridge bounds a single response, so a larger
/// logical read walks pages with the composite cursor.
///
/// Origin: `crates/buzz-cli/src/client.rs:498` (`QUERY_PAGE_SIZE`).
pub const QUERY_PAGE_SIZE: u32 = 500;

/// Default per-request total timeout, overridable with `BUZZ_TIMEOUT_SECS`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Default TCP connect timeout, overridable with `BUZZ_CONNECT_TIMEOUT_SECS`.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Moderation command kinds, whose retry policy is *not* the general one.
///
/// They execute at the relay before dedup, so a blind resend can duplicate the
/// mutation (§2.7). Carried here even though Wave 1 exposes no moderation
/// endpoint, so the endpoint cannot be added without the rule.
pub const MODERATION_KINDS: std::ops::RangeInclusive<u16> = 9040..=9044;

/// Sign a NIP-98 (kind 27235) HTTP auth event and return the header value.
///
/// Ported from `sign_nip98` (`crates/buzz-cli/src/client.rs:84`). The `nonce`
/// tag is a fresh UUID on **every** call, which is what makes a retry safe
/// against the relay's replay guard.
pub fn sign_nip98(keys: &Keys, method: &str, url: &str, body: Option<&[u8]>) -> Result<String> {
    let mut tags = vec![
        tag(["u", url])?,
        tag(["method", method])?,
        tag(["nonce", &uuid::Uuid::new_v4().to_string()])?,
    ];
    if let Some(body) = body {
        tags.push(tag(["payload", &hex::encode(Sha256::digest(body))])?);
    }
    let event = EventBuilder::new(Kind::Custom(27235), "")
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|e| DaemonError::Relay {
            status: 0,
            body: format!("NIP-98 signing failed: {e}"),
        })?;
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(event.as_json().as_bytes())
    ))
}

fn tag<'a, I: IntoIterator<Item = &'a str>>(parts: I) -> Result<Tag> {
    Tag::parse(parts).map_err(|e| DaemonError::Relay {
        status: 0,
        body: format!("tag error: {e}"),
    })
}

/// Full-jitter delay before attempt `attempt + 1`.
fn jitter_delay(attempt: u32) -> Duration {
    let ceiling = RETRY_BASE_SECS[(attempt as usize).min(RETRY_BASE_SECS.len() - 1)];
    Duration::from_secs_f64(ceiling * rand_unit())
}

/// A uniform sample in `[0, 1)`.
///
/// Deliberately not a `rand` dependency: jitter needs statistical spread, not
/// cryptographic quality, and the crate is not otherwise in this cone. Seeded
/// from the wall clock's nanoseconds, which is the same entropy source the
/// spread is defending against (many daemons retrying on the same tick).
fn rand_unit() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

/// Scan text for a `retry in <N>s` hint.
///
/// Ported from `parse_retry_hint_text` (`crates/buzz-cli/src/client.rs:156`),
/// including its requirement that the digit run be followed by a literal `s` —
/// without that, "retry in 4 minutes" parses as 4 seconds.
pub fn parse_retry_hint(text: &str) -> Option<u64> {
    const PREFIX: &str = "retry in ";
    let after = text.find(PREFIX).map(|i| &text[i + PREFIX.len()..])?;
    let end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    if end == 0 || after.as_bytes().get(end) != Some(&b's') {
        return None;
    }
    after[..end].parse::<u64>().ok()
}

/// Read a `u64`-seconds environment override, rejecting zero.
///
/// Zero is treated as invalid rather than as "no timeout": accidentally
/// disabling every timeout on a daemon that outlives its clients is a hang with
/// no diagnosis.
fn env_duration_secs(name: &str, default: u64) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map_or_else(|| Duration::from_secs(default), Duration::from_secs)
}

/// Advance a filter onto the next page using the **composite** cursor.
///
/// Ported from `advance_query_cursor` (`crates/buzz-cli/src/client.rs:500`),
/// including its id validation: a 64-char lowercase-hex check, because a
/// malformed `before_id` silently degrades the cursor back to `until`-only and
/// reintroduces the same-second duplicate/skip bug it exists to fix.
pub fn advance_query_cursor(filter: &mut serde_json::Value, page: &[serde_json::Value]) -> bool {
    let Some(last) = page.last() else {
        return false;
    };
    let Some(created_at) = last.get("created_at").and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let Some(id) = last
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit()))
    else {
        return false;
    };
    filter["until"] = serde_json::json!(created_at);
    filter["before_id"] = serde_json::json!(id);
    true
}

/// Whether an event kind is a moderation command with the non-idempotent
/// retry policy of §2.7.
pub fn is_moderation_kind(kind: u16) -> bool {
    MODERATION_KINDS.contains(&kind)
}

/// The relay's HTTP bridge: `POST /query`, `POST /count`, `POST /events`, plus
/// the public NIP-11 read.
pub struct RestClient {
    http: reqwest::Client,
    /// Base URL with no trailing slash, e.g. `https://relay.buzz.place`.
    base_url: String,
}

impl RestClient {
    /// Build a client against `base_url`.
    ///
    /// `base_url` is normalised to the HTTP origin: the daemon is configured
    /// with a `wss://` relay URL (that is what the socket-path preimage of §2.2
    /// hashes), and the bridge lives on the corresponding `https://` origin.
    pub fn new(base_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(env_duration_secs("BUZZ_TIMEOUT_SECS", DEFAULT_TIMEOUT_SECS))
            .connect_timeout(env_duration_secs(
                "BUZZ_CONNECT_TIMEOUT_SECS",
                DEFAULT_CONNECT_TIMEOUT_SECS,
            ))
            .build()
            .map_err(|e| DaemonError::Relay {
                status: 0,
                body: e.to_string(),
            })?;
        Ok(Self {
            http,
            base_url: http_origin(base_url),
        })
    }

    /// The HTTP origin this client posts to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `POST /query` with one filter.
    ///
    /// Every caller reaches the relay through here or [`Self::query_multi`], so
    /// the §2.4 kinds invariant is enforced in exactly one place rather than at
    /// each of a dozen call sites.
    pub async fn query(
        &self,
        identity: &Identity,
        filter: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>> {
        self.query_multi(identity, std::slice::from_ref(filter))
            .await
    }

    /// `POST /query` with several filters, ORed by the relay.
    pub async fn query_multi(
        &self,
        identity: &Identity,
        filters: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>> {
        for filter in filters {
            crate::search::assert_explicit_kinds(filter, "rest query")?;
        }
        let body = serde_json::to_vec(filters)?;
        let raw = self
            .post_authed(identity, "/query", bytes::Bytes::from(body))
            .await?;
        serde_json::from_str(&raw).map_err(|e| DaemonError::Relay {
            status: 0,
            body: format!("failed to parse query response: {e}"),
        })
    }

    /// Walk the composite cursor until `limit` events are collected or the
    /// relay runs out.
    ///
    /// `limit: None` reads everything. A short page terminates — this is the
    /// *bridge's* pagination, which is a different mechanism from NIP-CW's
    /// `39006` exhaustion authority ([D-10]); the two must not be confused.
    /// Here a short page really is the end, because `POST /query` has no
    /// server-assembled window and no `has_more`.
    pub async fn query_paginated(
        &self,
        identity: &Identity,
        mut filter: serde_json::Value,
        limit: Option<u32>,
    ) -> Result<Vec<serde_json::Value>> {
        let mut events: Vec<serde_json::Value> = Vec::new();
        while limit.is_none_or(|limit| events.len() < limit as usize) {
            let page_limit = limit
                .map(|limit| (limit as usize - events.len()).min(QUERY_PAGE_SIZE as usize))
                .unwrap_or(QUERY_PAGE_SIZE as usize);
            filter["limit"] = serde_json::json!(page_limit);

            let page = self.query(identity, &filter).await?;
            let done = page.len() < page_limit;
            if !done && !advance_query_cursor(&mut filter, &page) {
                // A page we cannot build a cursor from is the end of what we
                // can safely read. Continuing would re-request the same page
                // forever; guessing a cursor would skip events.
                events.extend(page);
                break;
            }
            events.extend(page);
            if done {
                break;
            }
        }
        Ok(events)
    }

    /// `POST /count`.
    pub async fn count(&self, identity: &Identity, filter: &serde_json::Value) -> Result<u64> {
        crate::search::assert_explicit_kinds(filter, "rest count")?;
        let body = serde_json::to_vec(&[filter])?;
        let raw = self
            .post_authed(identity, "/count", bytes::Bytes::from(body))
            .await?;
        let parsed: serde_json::Value = serde_json::from_str(&raw)?;
        Ok(parsed
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0))
    }

    /// `POST /events` — submit one signed event.
    ///
    /// Moderation kinds take the non-idempotent path of §2.7: an ambiguous
    /// outcome is [`DaemonError::DeliveryUnknown`] rather than a resend.
    pub async fn submit_event(&self, identity: &Identity, event: &nostr::Event) -> Result<String> {
        let body = bytes::Bytes::from(serde_json::to_vec(event)?);
        if is_moderation_kind(event.kind.as_u16()) {
            return self.post_authed_once(identity, "/events", body).await;
        }
        self.post_authed(identity, "/events", body).await
    }

    /// `GET` a public, unauthenticated endpoint — the NIP-11 `/info` document.
    ///
    /// No NIP-98 and no `x-auth-tag`: this is public relay metadata, not a
    /// membership-scoped resource, and signing it would leak the daemon's
    /// pubkey to an endpoint that has no business knowing it.
    pub async fn get_public(&self, path: &str) -> Result<String> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/nostr+json")
            .send()
            .await
            .map_err(network_error)?;
        Self::body_or_error(resp).await
    }

    /// `GET` an authenticated endpoint, retrying transient failures.
    pub async fn get_authed(&self, identity: &Identity, path: &str) -> Result<String> {
        let url = format!("{}{path}", self.base_url);
        let keys = identity
            .keys()
            .ok_or_else(|| DaemonError::IdentityDecrypt("no identity loaded".into()))?;
        for attempt in 0..RETRY_MAX_ATTEMPTS {
            let auth = sign_nip98(keys, "GET", &url, None)?;
            let req =
                self.with_auth_tag(identity, self.http.get(&url).header("Authorization", auth));
            match Self::send(req).await {
                Ok(body) => return Ok(body),
                Err(err) => match retry_delay(&err, attempt) {
                    Some(delay) if attempt + 1 < RETRY_MAX_ATTEMPTS => {
                        tokio::time::sleep(delay).await;
                    }
                    _ => return Err(err),
                },
            }
        }
        unreachable!("the loop returns on the final attempt")
    }

    /// `POST` with NIP-98, retrying transient failures.
    async fn post_authed(
        &self,
        identity: &Identity,
        path: &str,
        body: bytes::Bytes,
    ) -> Result<String> {
        let url = format!("{}{path}", self.base_url);
        let keys = identity
            .keys()
            .ok_or_else(|| DaemonError::IdentityDecrypt("no identity loaded".into()))?;
        for attempt in 0..RETRY_MAX_ATTEMPTS {
            // Re-signed per attempt: the fresh `nonce` is what keeps a retry
            // from tripping the relay's replay guard.
            let auth = sign_nip98(keys, "POST", &url, Some(&body))?;
            let req = self.with_auth_tag(
                identity,
                self.http
                    .post(&url)
                    .header("Authorization", auth)
                    .header("Content-Type", "application/json")
                    .body(body.clone()),
            );
            match Self::send(req).await {
                Ok(body) => return Ok(body),
                Err(err) => match retry_delay(&err, attempt) {
                    Some(delay) if attempt + 1 < RETRY_MAX_ATTEMPTS => {
                        tokio::time::sleep(delay).await;
                    }
                    _ => return Err(err),
                },
            }
        }
        unreachable!("the loop returns on the final attempt")
    }

    /// One attempt, no retry, ambiguity reported as such (§2.7).
    ///
    /// Used for moderation kinds, whose relay-side execution happens **before**
    /// dedup: a resend can duplicate the mutation, so an outcome we cannot
    /// observe is reported as `409 delivery_unknown` for the operator to check
    /// against the audit log, never retried.
    async fn post_authed_once(
        &self,
        identity: &Identity,
        path: &str,
        body: bytes::Bytes,
    ) -> Result<String> {
        let url = format!("{}{path}", self.base_url);
        let keys = identity
            .keys()
            .ok_or_else(|| DaemonError::IdentityDecrypt("no identity loaded".into()))?;
        let auth = sign_nip98(keys, "POST", &url, Some(&body))?;
        let req = self.with_auth_tag(
            identity,
            self.http
                .post(&url)
                .header("Authorization", auth)
                .header("Content-Type", "application/json")
                .body(body),
        );
        Self::send(req).await.map_err(|err| match &err {
            // Confirmed-unreceived: the request never left, so nothing executed.
            DaemonError::Relay { status: 0, .. } => err,
            DaemonError::Relay { status, body }
                if *status == 429 && body.contains("rate-limited:") =>
            {
                DaemonError::Relay {
                    status: *status,
                    body: body.clone(),
                }
            }
            _ => DaemonError::DeliveryUnknown,
        })
    }

    /// Attach the NIP-OA `x-auth-tag` header when one is configured.
    fn with_auth_tag(
        &self,
        identity: &Identity,
        req: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        match identity.auth_tag.as_ref() {
            Some(tag) => req.header("x-auth-tag", tag.raw.clone()),
            None => req,
        }
    }

    async fn send(req: reqwest::RequestBuilder) -> Result<String> {
        let resp = req.send().await.map_err(network_error)?;
        Self::body_or_error(resp).await
    }

    async fn body_or_error(resp: reqwest::Response) -> Result<String> {
        let status = resp.status();
        let body = resp.text().await.map_err(network_error)?;
        if status.is_success() {
            return Ok(body);
        }
        Err(DaemonError::Relay {
            status: status.as_u16(),
            body,
        })
    }
}

/// Map a transport failure onto `status: 0`, which the retry classifier reads
/// as "confirmed unreceived".
fn network_error(err: reqwest::Error) -> DaemonError {
    DaemonError::Relay {
        status: 0,
        body: err.to_string(),
    }
}

/// How long to wait before retrying, or `None` when the error is not retryable.
///
/// The classification is the CLI's: connect/timeout/body/decode failures and
/// `429 | 502 | 503 | 504`. A `429` carrying a `retry in Ns` hint uses the
/// hint, capped at [`RETRY_IN_MAX_SECS`] — an uncapped hint is a
/// relay-controlled hang.
pub fn retry_delay(err: &DaemonError, attempt: u32) -> Option<Duration> {
    match err {
        DaemonError::Relay { status: 0, .. } => Some(jitter_delay(attempt)),
        DaemonError::Relay { status: 429, body } => Some(
            parse_retry_hint(body)
                .map(|s| Duration::from_secs(s.min(RETRY_IN_MAX_SECS)))
                .unwrap_or_else(|| jitter_delay(attempt)),
        ),
        DaemonError::Relay {
            status: 502..=504, ..
        } => Some(jitter_delay(attempt)),
        _ => None,
    }
}

/// Normalise a relay URL to its HTTP origin, dropping any trailing slash.
///
/// The daemon is configured with the websocket URL because that is what §2.2's
/// socket-path preimage hashes; the bridge lives on the sibling HTTP origin.
pub fn http_origin(relay_url: &str) -> String {
    let trimmed = relay_url.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_json(created_at: u64, id: &str) -> serde_json::Value {
        serde_json::json!({"created_at": created_at, "id": id})
    }

    /// §5.2 `composite cursor`: same-second events are neither skipped nor
    /// repeated across a page boundary, which requires **both** fields.
    #[test]
    fn cursor_advance_sets_until_and_before_id() {
        let mut filter = serde_json::json!({"kinds": [9]});
        let page = vec![
            event_json(1_700_000_010, &"aa".repeat(32)),
            event_json(1_700_000_000, &"bb".repeat(32)),
        ];
        assert!(advance_query_cursor(&mut filter, &page));
        assert_eq!(filter["until"], serde_json::json!(1_700_000_000));
        assert_eq!(filter["before_id"], serde_json::json!("bb".repeat(32)));
    }

    /// A malformed id must not silently degrade the cursor to `until`-only —
    /// that is exactly the same-second bug the composite form exists to fix.
    #[test]
    fn a_malformed_id_refuses_to_advance_rather_than_degrading() {
        let mut filter = serde_json::json!({"kinds": [9]});
        assert!(!advance_query_cursor(
            &mut filter,
            &[event_json(1_700_000_000, "not-hex")]
        ));
        assert!(filter.get("until").is_none());
        assert!(filter.get("before_id").is_none());
    }

    #[test]
    fn an_empty_page_refuses_to_advance() {
        let mut filter = serde_json::json!({"kinds": [9]});
        assert!(!advance_query_cursor(&mut filter, &[]));
    }

    /// Ported hint parsing, including the trailing-`s` requirement.
    #[test]
    fn retry_hint_requires_the_seconds_suffix() {
        assert_eq!(parse_retry_hint("rate-limited: retry in 4s"), Some(4));
        assert_eq!(parse_retry_hint("retry in 4 minutes"), None);
        assert_eq!(parse_retry_hint("retry in s"), None);
        assert_eq!(parse_retry_hint("no hint here"), None);
    }

    /// §2.7: a relay-controlled hint cannot hang the daemon.
    #[test]
    fn a_pathological_retry_hint_is_capped() {
        let err = DaemonError::Relay {
            status: 429,
            body: "retry in 99999s".into(),
        };
        let delay = retry_delay(&err, 0).unwrap();
        assert_eq!(delay, Duration::from_secs(RETRY_IN_MAX_SECS));
    }

    /// The CLI's classification, ported: transient statuses retry, everything
    /// else surfaces.
    #[test]
    fn retry_classification_matches_the_cli() {
        for status in [0u16, 429, 502, 503, 504] {
            let err = DaemonError::Relay {
                status,
                body: String::new(),
            };
            assert!(retry_delay(&err, 0).is_some(), "{status} should retry");
        }
        for status in [400u16, 401, 403, 404, 409, 500] {
            let err = DaemonError::Relay {
                status,
                body: String::new(),
            };
            assert!(retry_delay(&err, 0).is_none(), "{status} must not retry");
        }
    }

    /// §2.7: moderation kinds carry a different policy, and the range is the
    /// registry's.
    #[test]
    fn moderation_kinds_are_9040_through_9044() {
        for kind in 9040u16..=9044 {
            assert!(is_moderation_kind(kind), "{kind}");
        }
        assert!(!is_moderation_kind(9039));
        assert!(!is_moderation_kind(9045));
        assert!(!is_moderation_kind(9));
    }

    /// §2.2 hashes the websocket URL; the bridge is the sibling HTTP origin.
    #[test]
    fn websocket_urls_map_onto_their_http_origin() {
        assert_eq!(http_origin("wss://relay.example/"), "https://relay.example");
        assert_eq!(http_origin("ws://localhost:3000"), "http://localhost:3000");
        assert_eq!(
            http_origin("https://relay.example"),
            "https://relay.example"
        );
    }

    /// `sign_nip98` must produce a *fresh* nonce per call, or a retry trips the
    /// relay's replay guard and a transient 503 becomes an auth failure.
    #[test]
    fn nip98_signatures_are_unique_per_call() {
        let keys = Keys::generate();
        let a = sign_nip98(&keys, "POST", "https://relay.example/query", Some(b"{}")).unwrap();
        let b = sign_nip98(&keys, "POST", "https://relay.example/query", Some(b"{}")).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("Nostr "), "{a}");
    }

    /// The signed event must carry the payload hash, or the relay rejects a
    /// body-bearing request as unauthenticated.
    #[test]
    fn nip98_carries_url_method_and_payload_hash() {
        let keys = Keys::generate();
        let header =
            sign_nip98(&keys, "POST", "https://relay.example/query", Some(b"body")).unwrap();
        let json = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Nostr ").unwrap())
            .unwrap();
        let event: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let tags = event["tags"].as_array().unwrap();
        let has = |name: &str| tags.iter().any(|t| t[0] == name);
        assert!(
            has("u") && has("method") && has("nonce") && has("payload"),
            "{event}"
        );
        let expected = hex::encode(Sha256::digest(b"body"));
        assert!(
            tags.iter()
                .any(|t| t[0] == "payload" && t[1] == expected.as_str()),
            "{event}"
        );
    }

    /// A GET has no body, so it must carry no `payload` tag — a hash of nothing
    /// is not the same as no hash, and the relay checks for absence.
    #[test]
    fn a_bodyless_request_omits_the_payload_tag() {
        let keys = Keys::generate();
        let header = sign_nip98(&keys, "GET", "https://relay.example/info", None).unwrap();
        let json = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Nostr ").unwrap())
            .unwrap();
        let event: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert!(
            !event["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t[0] == "payload"),
            "{event}"
        );
    }

    /// §2.4's global invariant is enforced at the one chokepoint every read
    /// goes through, not at each call site.
    #[tokio::test]
    async fn a_kindless_filter_never_reaches_the_wire() {
        use zeroize::Zeroizing;
        let client = RestClient::new("wss://relay.example").unwrap();
        let identity = Identity::new("aa".repeat(32), Zeroizing::new(vec![7u8; 32]), None);
        let err = client
            .query(&identity, &serde_json::json!({"#h": ["chan"]}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "kindless_filter");
    }
}
