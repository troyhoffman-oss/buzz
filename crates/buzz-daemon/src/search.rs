//! Full-text search over NIP-50, and the global kinds invariant.
//!
//! Implements Wave-1 daemon deliverable 7 (`DESIGN.md` §4.1.1) and §3.5.
//!
//! # The kinds invariant is global, not search-specific
//!
//! §2.4: **no filter leaves the daemon without an explicit `kinds`.** §3.5
//! previously stated this as a search rule, which would mislead an implementer
//! into setting `kinds` on `/search` and forgetting it on `/message/{id}`. The
//! actual gate is `p_gated_filters_authorized`
//! (`crates/buzz-relay/src/handlers/req.rs` → `crates/buzz-core/src/kind.rs`):
//! a filter that *can match* any `P_GATED_KIND` is refused unless its `#p`
//! values equal the authenticated reader's pubkey — and a filter with **no**
//! `kinds` can match everything. It applies to `REQ` and `/query` equally,
//! closing as `restricted:` or `403` depending on transport.
//!
//! The `ids` exemption has two carve-outs that bite a Wave-1 endpoint:
//! `RESULT_GATED_KINDS` (`KIND_DM_VISIBILITY`, `KIND_AGENT_TURN_METRIC`) lose
//! the exemption when named explicitly, so `/agent/{pk}/metric` **must** carry
//! `#p = self` — see [`crate::metric`].

use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};

/// Default search kinds (§3.5).
///
/// An open-ended search would hit the relay's p-gate and return 403, so this is
/// never empty.
pub const DEFAULT_SEARCH_KINDS: [u32; 4] = [9, 40002, 45001, 45003];

/// Search-as-you-type debounce, in milliseconds. Protects the relay's FTS, not
/// just the render loop (§3.5).
pub const SEARCH_DEBOUNCE_MS: u64 = 150;

/// Parsed Slack-style search operators (§3.5).
///
/// TODO(wave1, §4.1.1 deliverable 7): parse identically to the desktop's
/// `parseSearchOperators.ts`. Operators must start at a **token boundary** —
/// deliberately not `\b`, so `built-in:react` and `https://x.com/in:foo` are not
/// misparsed. `after:` is local start-of-day inclusive; `before:` is one second
/// before local start-of-day, because NIP-01 `until` is inclusive and Slack
/// excludes the named day. An invalid operator value stays in the FTS text
/// rather than erroring.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Residual free text after operators are lifted out.
    pub text: String,
    /// `from:` — may be a display name, which is what makes
    /// [`AmbiguousAuthor`] necessary.
    pub from: Option<String>,
    /// `in:` — channel scope.
    pub in_channel: Option<String>,
    /// `after:` as local start-of-day, inclusive.
    pub after: Option<i64>,
    /// `before:` as local start-of-day minus one second.
    pub before: Option<i64>,
}

/// The `409 ambiguous_author` body of §3.5.
///
/// Rendered as a disambiguation list — **never a silent mix of authors**.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousAuthor {
    /// The name that matched more than one identity.
    pub query: String,
    /// Every candidate, so the client can disambiguate without a second round
    /// trip.
    pub candidates: Vec<String>,
}

/// Maximum results a single search returns, per `daemon-api.md` §3.4.
pub const SEARCH_LIMIT_CAP: u32 = 100;

/// Parse Slack-style search operators out of a free-text query.
///
/// Ported from `parseSearchOperators`
/// (`desktop/src/features/search/lib/parseSearchOperators.ts`), including the
/// three rules §5.2's `search operator parse` row names:
///
/// - **Token-boundary start, deliberately not `\b`.** A word boundary also
///   matches after `-` and `/`, which would turn `built-in:react` and
///   `https://x.com/in:foo` into operators. An operator must begin at
///   start-of-input or after whitespace.
/// - **`after:` is local start-of-day, inclusive.**
/// - **`before:` is start-of-day minus one second**, because NIP-01 `until` is
///   an *inclusive* upper bound and Slack excludes the named day.
///
/// An operator with an unparseable value **stays in the FTS text** rather than
/// erroring: `after:yesterday` is far more likely to be a search for the word
/// than a malformed date, and rejecting the query teaches the operator to avoid
/// the feature.
pub fn parse_operators(raw: &str) -> SearchQuery {
    let mut query = SearchQuery::default();
    let mut kept: Vec<String> = Vec::new();

    for token in split_tokens(raw) {
        let Some((name, value)) = token.split_once(':') else {
            kept.push(token.to_string());
            continue;
        };
        let value = clean_operator_value(value);
        if value.is_empty() {
            kept.push(token.to_string());
            continue;
        }
        // Later occurrences win, matching the desktop.
        match name.to_lowercase().as_str() {
            "from" => query.from = Some(strip_sigil(&value, '@')),
            "in" => query.in_channel = Some(strip_sigil(&value, '#')),
            "after" => match parse_local_day_start(&value) {
                Some(ts) => query.after = Some(ts),
                None => kept.push(token.to_string()),
            },
            "before" => match parse_local_day_start(&value) {
                // NIP-01 `until` is inclusive, so step back one second to keep
                // `before:` exclusive of the named day.
                Some(ts) => query.before = Some(ts - 1),
                None => kept.push(token.to_string()),
            },
            _ => kept.push(token.to_string()),
        }
    }

    query.text = kept.join(" ").trim().to_string();
    query
}

/// Split on ASCII whitespace, which is exactly the token boundary the
/// `(?:^|\s)` anchor encodes.
fn split_tokens(raw: &str) -> Vec<&str> {
    raw.split_ascii_whitespace().collect()
}

/// Drop trailing punctuation, so `in:general,` still resolves to `general`.
fn clean_operator_value(value: &str) -> String {
    value
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .to_string()
}

fn strip_sigil(value: &str, sigil: char) -> String {
    value.strip_prefix(sigil).unwrap_or(value).to_string()
}

/// Parse `YYYY-MM-DD` into unix seconds at **local** start of day.
///
/// Local, not UTC: an operator typing `after:2026-08-04` means their own
/// Monday. Resolving it in UTC shifts the boundary by up to a day and silently
/// drops or includes a day's messages.
fn parse_local_day_start(value: &str) -> Option<i64> {
    use chrono::TimeZone as _;
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    let naive = date.and_hms_opt(0, 0, 0)?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.timestamp())
}

/// Whether a value is a 64-char hex pubkey.
pub fn is_hex_pubkey(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve a `from:` value to a single author pubkey.
///
/// A hex pubkey or npub resolves to itself. A **display name** is resolved
/// against the directory, and a name matching more than one identity is
/// [`DaemonError::AmbiguousAuthor`] — never a silent mix of authors, which is
/// the failure mode that makes a search result quietly wrong rather than
/// visibly empty.
pub fn resolve_author(
    value: &str,
    directory: &std::collections::BTreeMap<String, crate::mentions::Profile>,
) -> Result<String> {
    if is_hex_pubkey(value) {
        return Ok(value.to_lowercase());
    }
    if value.starts_with("npub1") {
        use nostr::FromBech32 as _;
        return nostr::PublicKey::from_bech32(value)
            .map(|pk| pk.to_hex())
            .map_err(|_| DaemonError::InvalidInput(format!("not a valid npub: {value}")));
    }

    let needle = value.to_lowercase();
    let matches: Vec<String> = directory
        .values()
        .filter(|profile| profile.label().to_lowercase() == needle)
        .map(|profile| profile.pubkey.clone())
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("length checked")),
        0 => Err(DaemonError::NotFound(format!("no author named {value:?}"))),
        _ => Err(DaemonError::AmbiguousAuthor {
            query: value.to_string(),
            candidates: matches,
        }),
    }
}

/// Build the full search filter from a parsed query.
///
/// `limit` is clamped to [`SEARCH_LIMIT_CAP`], and `kinds` is always set.
pub fn build_query_filter(
    query: &SearchQuery,
    author: Option<&str>,
    kinds: Option<&[u32]>,
    limit: u32,
) -> serde_json::Value {
    let mut filter = serde_json::json!({
        "kinds": kinds.unwrap_or(&DEFAULT_SEARCH_KINDS),
        "limit": limit.clamp(1, SEARCH_LIMIT_CAP),
    });
    // An empty residual text is omitted rather than sent as `search: ""`: an
    // empty NIP-50 search is not the same query as no search, and the relay's
    // FTS path treats it differently from the plain one.
    if !query.text.is_empty() {
        filter["search"] = serde_json::json!(query.text);
    }
    if let Some(author) = author {
        filter["authors"] = serde_json::json!([author]);
    }
    if let Some(channel) = query.in_channel.as_deref() {
        filter["#h"] = serde_json::json!([channel]);
    }
    if let Some(after) = query.after {
        filter["since"] = serde_json::json!(after);
    }
    if let Some(before) = query.before {
        filter["until"] = serde_json::json!(before);
    }
    filter
}

/// Whether results should be ranked by relevance or by recency (§3.4).
///
/// Relevance needs a relevance *signal*; with only `from:` or `after:` given
/// there is none, and ranking by a score every row ties on produces an order
/// that looks meaningful and is not.
pub fn ranking_for(query: &SearchQuery) -> &'static str {
    if query.text.is_empty() {
        "recency"
    } else {
        "relevance"
    }
}

/// Assert a filter carries explicit `kinds` before it leaves the daemon.
///
/// The global invariant of §2.4, enforced here so no endpoint can forget it.
pub fn assert_explicit_kinds(filter: &serde_json::Value, context: &'static str) -> Result<()> {
    let kinds = filter.get("kinds").and_then(|k| k.as_array());
    match kinds {
        Some(k) if !k.is_empty() => Ok(()),
        _ => Err(DaemonError::KindlessFilter { context }),
    }
}

/// Build a search filter with `kinds` always set.
pub fn build_search_filter(query: &str, kinds: Option<&[u32]>) -> serde_json::Value {
    let kinds = kinds.unwrap_or(&DEFAULT_SEARCH_KINDS);
    serde_json::json!({
        "search": query,
        "kinds": kinds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §3.5 fixes the default kind set.
    #[test]
    fn default_kinds_match_the_design() {
        assert_eq!(DEFAULT_SEARCH_KINDS, [9, 40002, 45001, 45003]);
    }

    /// §2.4: a kindless filter is refused before it reaches the relay's p-gate.
    #[test]
    fn kindless_filters_are_refused() {
        let err = assert_explicit_kinds(&serde_json::json!({"search": "x"}), "search").unwrap_err();
        assert_eq!(err.code(), "kindless_filter");
    }

    /// An empty `kinds` array can match everything just as a missing one can.
    #[test]
    fn empty_kinds_array_is_also_refused() {
        let filter = serde_json::json!({"kinds": [], "search": "x"});
        assert!(assert_explicit_kinds(&filter, "search").is_err());
    }

    #[test]
    fn a_built_search_filter_satisfies_the_invariant() {
        let filter = build_search_filter("read-state slots", None);
        assert_explicit_kinds(&filter, "search").unwrap();
        assert_eq!(filter["kinds"], serde_json::json!(DEFAULT_SEARCH_KINDS));
    }

    #[test]
    fn explicit_kinds_override_the_default() {
        let filter = build_search_filter("q", Some(&[9]));
        assert_eq!(filter["kinds"], serde_json::json!([9]));
    }

    #[test]
    fn debounce_matches_the_design() {
        assert_eq!(SEARCH_DEBOUNCE_MS, 150);
    }

    // ── §5.2 `search operator parse` ──────────────────────────────────────

    /// §5.2: "token-boundary rule (`built-in:react` not parsed)."
    ///
    /// A `\b` word boundary also matches after `-` and `/`, which would turn
    /// these into operators. This is the single most consequential detail in
    /// the parser, because the failure is silent: the query returns wrong
    /// results rather than an error.
    #[test]
    fn operators_require_a_token_boundary_not_a_word_boundary() {
        let parsed = parse_operators("built-in:react");
        assert_eq!(parsed.in_channel, None, "`built-in:` is not an `in:`");
        assert_eq!(parsed.text, "built-in:react");

        let parsed = parse_operators("https://x.com/in:foo");
        assert_eq!(parsed.in_channel, None);
        assert_eq!(parsed.text, "https://x.com/in:foo");
    }

    #[test]
    fn an_operator_at_the_start_or_after_whitespace_parses() {
        assert_eq!(
            parse_operators("in:engineering").in_channel.as_deref(),
            Some("engineering")
        );
        assert_eq!(
            parse_operators("read state in:engineering")
                .in_channel
                .as_deref(),
            Some("engineering")
        );
    }

    /// §5.2: "`after:` inclusive local SOD; `before:` = SOD−1s."
    ///
    /// The one-second step-back is what makes `before:` exclude the named day:
    /// NIP-01's `until` is inclusive, so without it `before:2026-08-04` would
    /// return the whole of the 4th.
    #[test]
    fn date_operators_use_local_start_of_day_with_before_stepped_back() {
        let after = parse_operators("after:2026-08-04").after.unwrap();
        let before = parse_operators("before:2026-08-04").before.unwrap();
        assert_eq!(
            before,
            after - 1,
            "before: is one second earlier than the same day's after:"
        );

        // And it really is *local* midnight, not UTC midnight.
        use chrono::TimeZone as _;
        let expected = chrono::Local
            .with_ymd_and_hms(2026, 8, 4, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert_eq!(after, expected);
    }

    /// §5.2: "invalid value stays in FTS text." `after:yesterday` is far more
    /// likely to be a search for the word than a malformed date.
    #[test]
    fn an_unparseable_date_stays_in_the_search_text() {
        let parsed = parse_operators("after:yesterday deploy");
        assert_eq!(parsed.after, None);
        assert_eq!(parsed.text, "after:yesterday deploy");
    }

    #[test]
    fn an_impossible_date_stays_in_the_search_text() {
        let parsed = parse_operators("before:2026-13-45");
        assert_eq!(parsed.before, None);
        assert_eq!(parsed.text, "before:2026-13-45");
    }

    /// Trailing punctuation is dropped, so `in:general,` still resolves.
    #[test]
    fn trailing_punctuation_is_stripped_from_operator_values() {
        assert_eq!(
            parse_operators("in:general, please").in_channel.as_deref(),
            Some("general")
        );
    }

    /// The sigils operators are typed with are not part of the value.
    #[test]
    fn leading_sigils_are_stripped() {
        assert_eq!(parse_operators("from:@matt").from.as_deref(), Some("matt"));
        assert_eq!(
            parse_operators("in:#engineering").in_channel.as_deref(),
            Some("engineering")
        );
    }

    /// Later occurrences win, matching the desktop — a second `in:` is a
    /// correction, not a second constraint.
    #[test]
    fn a_repeated_operator_takes_the_last_value() {
        assert_eq!(
            parse_operators("in:general in:engineering")
                .in_channel
                .as_deref(),
            Some("engineering")
        );
    }

    /// Everything that is not an operator survives as the FTS query, collapsed
    /// to single spaces.
    #[test]
    fn residual_text_is_the_fts_query() {
        let parsed = parse_operators("  read   state from:matt  slots  ");
        assert_eq!(parsed.text, "read state slots");
        assert_eq!(parsed.from.as_deref(), Some("matt"));
    }

    /// A bare colon token is not an operator — it has no value.
    #[test]
    fn an_operator_with_no_value_stays_in_the_text() {
        let parsed = parse_operators("from:");
        assert_eq!(parsed.from, None);
        assert_eq!(parsed.text, "from:");
    }

    // ── Author resolution and `409 ambiguous_author` ──────────────────────

    fn directory_with(
        names: &[(&str, &str)],
    ) -> std::collections::BTreeMap<String, crate::mentions::Profile> {
        names
            .iter()
            .map(|(tag, name)| {
                let pubkey = format!("{tag:0<64}");
                (
                    pubkey.clone(),
                    crate::mentions::Profile {
                        pubkey,
                        display_name: Some((*name).to_string()),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_hex_pubkey_resolves_to_itself() {
        let directory = directory_with(&[]);
        let hex = "ab".repeat(32);
        assert_eq!(resolve_author(&hex, &directory).unwrap(), hex);
    }

    #[test]
    fn an_npub_resolves_to_its_hex() {
        use nostr::ToBech32 as _;
        let keys = nostr::Keys::generate();
        let npub = keys.public_key().to_bech32().unwrap();
        let directory = directory_with(&[]);
        assert_eq!(
            resolve_author(&npub, &directory).unwrap(),
            keys.public_key().to_hex()
        );
    }

    #[test]
    fn a_unique_display_name_resolves() {
        let directory = directory_with(&[("a", "matt")]);
        assert_eq!(
            resolve_author("matt", &directory).unwrap(),
            format!("{:0<64}", "a")
        );
    }

    /// §3.5: a name matching more than one identity is `409 ambiguous_author`
    /// **with the candidate list** — never a silent mix of authors, which
    /// makes a search result quietly wrong rather than visibly empty.
    #[test]
    fn an_ambiguous_name_is_refused_with_its_candidates() {
        let directory = directory_with(&[("a", "matt"), ("b", "matt")]);
        let err = resolve_author("matt", &directory).unwrap_err();
        assert_eq!(err.code(), "ambiguous_author");
        let body = crate::error::ErrorBody::from(&err);
        let candidates = body.detail.unwrap()["candidates"].as_array().unwrap().len();
        assert_eq!(
            candidates, 2,
            "the client disambiguates without a round trip"
        );
    }

    #[test]
    fn an_unknown_name_is_not_found_rather_than_ambiguous() {
        let directory = directory_with(&[("a", "matt")]);
        assert_eq!(
            resolve_author("nobody", &directory).unwrap_err().code(),
            "not_found"
        );
    }

    // ── The built filter ──────────────────────────────────────────────────

    #[test]
    fn the_built_filter_carries_every_parsed_constraint() {
        let query = parse_operators("read state in:engineering after:2026-08-01");
        let author = "aa".repeat(32);
        let filter = build_query_filter(&query, Some(&author), None, 50);
        assert_explicit_kinds(&filter, "search").unwrap();
        assert_eq!(filter["search"], serde_json::json!("read state"));
        assert_eq!(filter["#h"], serde_json::json!(["engineering"]));
        assert_eq!(filter["authors"], serde_json::json!([author]));
        assert!(filter.get("since").is_some());
    }

    /// `limit` is clamped, so a client cannot ask the relay's FTS for ten
    /// thousand rows.
    #[test]
    fn the_limit_is_clamped_in_both_directions() {
        let query = parse_operators("x");
        assert_eq!(
            build_query_filter(&query, None, None, 10_000)["limit"],
            serde_json::json!(SEARCH_LIMIT_CAP)
        );
        assert_eq!(
            build_query_filter(&query, None, None, 0)["limit"],
            serde_json::json!(1)
        );
    }

    /// An empty residual text is **omitted**, not sent as `search: ""`: an
    /// empty NIP-50 search is a different query from no search, and the relay
    /// routes the two down different paths.
    #[test]
    fn an_operator_only_query_omits_the_search_key() {
        let query = parse_operators("from:matt");
        let filter = build_query_filter(&query, Some(&"aa".repeat(32)), None, 50);
        assert!(filter.get("search").is_none(), "{filter}");
        assert_explicit_kinds(&filter, "search").unwrap();
    }

    /// §3.4: relevance needs a relevance *signal*. Ranking by a score every row
    /// ties on produces an order that looks meaningful and is not.
    #[test]
    fn ranking_falls_back_to_recency_without_a_text_query() {
        assert_eq!(ranking_for(&parse_operators("read state")), "relevance");
        assert_eq!(ranking_for(&parse_operators("from:matt")), "recency");
        assert_eq!(ranking_for(&parse_operators("after:2026-08-01")), "recency");
    }
}
