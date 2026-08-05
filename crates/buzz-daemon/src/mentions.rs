//! Mention candidates and the mention inbox.
//!
//! Implements Wave-1 daemon deliverable 6 (`DESIGN.md` §4.1.1) and the daemon
//! half of §2.4 [D-2].
//!
//! # [D-2] The TUI sends resolved pubkeys, never names
//!
//! `daemon-api.md` §3.3 allows `POST /channel/{id}/message` to omit `mentions`
//! and have the daemon run `extract_at_mentions_with_known` server-side. That
//! path stays for `curl` and for second clients, but the TUI **must not use
//! it**. `GET /mention/candidates` returns candidates that already carry their
//! pubkey; the composer's parts array holds the pubkey; the send carries an
//! explicit `mentions: [pubkey…]`.
//!
//! Two payoffs: "what you picked is what gets tagged" becomes true by
//! construction rather than by two implementations agreeing, and **frecency
//! ranking can live in the TUI** — where it belongs, since it is per-front-end
//! UI personalization, not protocol — with no risk of the ranked pick and the
//! resolved tag diverging. (Contrast drafts, which are daemon-held precisely
//! because they are per-*identity*; see §4.1.2.)

use serde::{Deserialize, Serialize};

/// Hard upper bound on `p` tags per message.
///
/// Origin: `crates/buzz-sdk/src/mentions.rs:38` (`MENTION_CAP`). This is a
/// **build-time** rejection in the SDK (`SdkError::TooManyMentions`), so it must
/// be surfaced *before* the send, not after: the daemon returns
/// `400 too_many_mentions {cap: 50, requested: n}` and the composer shows a
/// live `n of 50` counter at pick time. Failing at Enter on a message the
/// operator has already written is the worst possible place to learn about a
/// cap (§2.4).
pub const MENTION_CAP: usize = 50;

/// A mention candidate, carrying its **resolved pubkey** per [D-2].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MentionCandidate {
    /// Lowercase-hex pubkey. Present on every candidate — this is what makes
    /// "what you picked is what gets tagged" true by construction.
    pub pubkey: String,
    /// Display name shown in the picker.
    pub display_name: String,
    /// Whether this candidate came from the channel roster (which outranks the
    /// directory — §5.2's `candidate ranking` row).
    pub in_roster: bool,
    /// Whether this candidate is an agent rather than a human.
    pub is_agent: bool,
}

/// The `400 too_many_mentions` body of §2.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TooManyMentions {
    /// Always [`MENTION_CAP`].
    pub cap: usize,
    /// How many the client asked for.
    pub requested: usize,
}

/// Reject a mention list that would fail the SDK's build-time cap, *before* the
/// send.
pub fn check_mention_cap(requested: usize) -> Result<(), TooManyMentions> {
    if requested > MENTION_CAP {
        return Err(TooManyMentions {
            cap: MENTION_CAP,
            requested,
        });
    }
    Ok(())
}

/// A profile as the directory holds it, for candidate resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    /// Lowercase-hex pubkey.
    pub pubkey: String,
    /// `name` from the kind-0 content.
    pub name: Option<String>,
    /// `display_name` from the kind-0 content.
    pub display_name: Option<String>,
    /// Whether this identity is an agent (kind 10100 / 30177 registered).
    pub is_agent: bool,
}

impl Profile {
    /// The name the picker shows and the matcher matches on.
    ///
    /// `display_name` wins over `name` because that is the field a human chose
    /// to be called; falling back to the pubkey prefix rather than to "unknown"
    /// keeps two unnamed identities distinguishable in the picker.
    pub fn label(&self) -> String {
        self.display_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .or(self.name.as_deref().filter(|n| !n.is_empty()))
            .map(str::to_string)
            .unwrap_or_else(|| self.pubkey.chars().take(8).collect())
    }
}

/// Rank mention candidates for `prefix` within a channel (§4.1.1 deliverable 6).
///
/// Ordering, in priority order:
///
/// 1. **Roster before directory.** §5.2's `candidate ranking` row: "roster
///    outranks directory". Someone in the channel is overwhelmingly more likely
///    to be who you meant than someone who merely exists.
/// 2. **Prefix match before substring.** A prefix match is what the operator is
///    typing; a substring match is a guess.
/// 3. **Case-insensitive label**, then pubkey, so the order is **total** — a
///    picker whose rows permute between keystrokes moves the selection out from
///    under the operator's fingers.
///
/// **Frecency is deliberately absent.** [D-2] keeps it in the TUI because it is
/// per-front-end UI personalization, not protocol — and because [D-2]'s whole
/// point is that the *picked* candidate carries its pubkey, so a client-side
/// re-rank cannot make the ranked pick and the resolved tag diverge.
pub fn rank_candidates(
    prefix: &str,
    roster: &std::collections::BTreeSet<String>,
    directory: &std::collections::BTreeMap<String, Profile>,
    limit: usize,
) -> Vec<MentionCandidate> {
    rank_with(prefix, directory, limit, |pubkey| roster.contains(pubkey))
}

/// Rank across the **whole directory**, with everyone treated as in-roster.
///
/// The directory-wide search of `GET /user` and `GET /search/user`, where there
/// is no channel to be a member of. It exists so those two handlers do not have
/// to fake a roster by cloning every key into a `BTreeSet` — an allocation
/// proportional to the directory, per request, **under the daemon's single
/// mutex**, which is the one place this process cannot afford one.
pub fn rank_directory(
    prefix: &str,
    directory: &std::collections::BTreeMap<String, Profile>,
    limit: usize,
) -> Vec<MentionCandidate> {
    rank_with(prefix, directory, limit, |_| true)
}

/// The shared ranking core. `in_roster` is a predicate rather than a set so the
/// directory-wide case costs nothing to express.
fn rank_with(
    prefix: &str,
    directory: &std::collections::BTreeMap<String, Profile>,
    limit: usize,
    in_roster: impl Fn(&str) -> bool,
) -> Vec<MentionCandidate> {
    let needle = prefix.to_lowercase();
    let mut scored: Vec<(u8, String, MentionCandidate)> = directory
        .values()
        .filter_map(|profile| {
            let label = profile.label();
            let haystack = label.to_lowercase();
            // An empty prefix lists the roster — the picker opens on `@` with
            // no characters typed yet, and an empty list there reads as "no one
            // to mention" rather than "keep typing".
            let rank = if needle.is_empty() {
                2
            } else if haystack.starts_with(&needle) {
                0
            } else if haystack.contains(&needle) {
                1
            } else {
                return None;
            };
            let in_roster = in_roster(&profile.pubkey);
            if needle.is_empty() && !in_roster {
                return None;
            }
            Some((
                // Roster membership dominates the match quality: a roster
                // substring match outranks a directory prefix match, because
                // the person is *here*.
                if in_roster { rank } else { rank + 8 },
                haystack,
                MentionCandidate {
                    pubkey: profile.pubkey.clone(),
                    display_name: label,
                    in_roster,
                    is_agent: profile.is_agent,
                },
            ))
        })
        .collect();

    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.pubkey.cmp(&b.2.pubkey))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, candidate)| candidate)
        .collect()
}

/// Build the mention-inbox filter: messages tagging me.
///
/// `kinds` is explicit per §2.4, and scoped to the **conversational** kinds:
/// a job-lifecycle event that happens to carry a `p` tag is not a mention, and
/// listing it in the inbox trains the operator to ignore the inbox.
pub fn build_inbox_filter(self_pubkey: &str, since: Option<u64>, limit: u32) -> serde_json::Value {
    let mut filter = serde_json::json!({
        "kinds": crate::timeline::CONVERSATIONAL_KINDS,
        "#p": [self_pubkey],
        "limit": limit,
    });
    if let Some(since) = since {
        filter["since"] = serde_json::json!(since);
    }
    filter
}

/// Resolve `@name` mentions server-side from message text.
///
/// The `curl` and second-client path of [D-2]. **The TUI must not use it** —
/// it sends resolved pubkeys, so "what you picked is what gets tagged" holds by
/// construction rather than by two implementations agreeing.
///
/// Delegates to `buzz_sdk::mentions::extract_at_mentions_with_known`, which
/// handles multi-word display names longest-first. Reimplementing that matcher
/// here is exactly the divergence [D-2] exists to prevent.
pub fn resolve_from_text(
    content: &str,
    directory: &std::collections::BTreeMap<String, Profile>,
    roster: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let labels: Vec<String> = roster
        .iter()
        .filter_map(|pubkey| directory.get(pubkey))
        .map(|profile| profile.label())
        .collect();
    let known: Vec<&str> = labels.iter().map(String::as_str).collect();
    let names = buzz_sdk::mentions::extract_at_mentions_with_known(content, &known);

    let by_label: std::collections::BTreeMap<String, &str> = roster
        .iter()
        .filter_map(|pubkey| directory.get(pubkey))
        .map(|profile| (profile.label().to_lowercase(), profile.pubkey.as_str()))
        .collect();
    let mut resolved: Vec<String> = Vec::new();
    for name in names {
        if let Some(pubkey) = by_label.get(&name.to_lowercase()) {
            let pubkey = (*pubkey).to_string();
            if !resolved.contains(&pubkey) {
                resolved.push(pubkey);
            }
        }
    }
    resolved
}

/// Mention candidate resolution over the roster cache.
///
/// **`@channel` is deferred out of Wave 1** (§2.4). There is no wire
/// representation for it anywhere: no handling in `buzz-sdk`, none in the
/// relay, none in `desktop/src`. Client-side expansion to N `p` tags works at
/// 27 members and hard-fails at 51, which makes it a feature that breaks as a
/// community grows. It returns when it is specified as protocol — a marker tag
/// the relay expands, with fan-out accounted at the relay — not before. Tracked
/// as Q8 (§7.2).
#[derive(Debug, Default)]
pub struct Mentions {
    /// Profiles by pubkey, the directory candidates are drawn from.
    pub directory: std::collections::BTreeMap<String, Profile>,
}

impl Mentions {
    /// An empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record or update a profile.
    pub fn upsert(&mut self, profile: Profile) {
        self.directory.insert(profile.pubkey.clone(), profile);
    }

    /// Candidates for `GET /mention/candidates`.
    pub fn candidates(
        &self,
        prefix: &str,
        roster: &std::collections::BTreeSet<String>,
        limit: usize,
    ) -> Vec<MentionCandidate> {
        rank_candidates(prefix, roster, &self.directory, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.4 cites `crates/buzz-sdk/src/mentions.rs:38`.
    #[test]
    fn cap_matches_the_sdk() {
        assert_eq!(MENTION_CAP, 50);
    }

    /// §2.4: the cap is surfaced before the send, not after.
    #[test]
    fn cap_is_inclusive_and_reports_both_numbers() {
        check_mention_cap(MENTION_CAP).unwrap();
        let err = check_mention_cap(MENTION_CAP + 1).unwrap_err();
        assert_eq!(err.cap, 50);
        assert_eq!(err.requested, 51);
    }

    /// [D-2]: every candidate carries a pubkey, so the picked identity and the
    /// tagged identity cannot diverge.
    #[test]
    fn candidates_carry_a_resolved_pubkey() {
        let json = serde_json::to_string(&MentionCandidate {
            pubkey: "aa".repeat(32),
            display_name: "matt".into(),
            in_roster: true,
            is_agent: false,
        })
        .unwrap();
        assert!(json.contains("pubkey"), "{json}");
    }

    // ── §5.2 `candidate ranking` ──────────────────────────────────────────

    fn pk(tag: &str) -> String {
        format!("{tag:0<64}")
    }

    fn directory() -> (
        std::collections::BTreeMap<String, Profile>,
        std::collections::BTreeSet<String>,
    ) {
        let people = [
            ("matt", "Matt Rickard", false),
            ("marge", "Margaret", false),
            ("claude", "claude-1", true),
            ("stranger", "Matthias", false),
        ];
        let mut directory = std::collections::BTreeMap::new();
        for (tag, name, is_agent) in people {
            directory.insert(
                pk(tag),
                Profile {
                    pubkey: pk(tag),
                    name: Some(name.into()),
                    display_name: Some(name.into()),
                    is_agent,
                },
            );
        }
        // "stranger" exists in the directory but is not in this channel.
        let roster: std::collections::BTreeSet<String> = [pk("matt"), pk("marge"), pk("claude")]
            .into_iter()
            .collect();
        (directory, roster)
    }

    /// §5.2: **roster outranks directory.** Someone in the channel is
    /// overwhelmingly more likely to be who you meant.
    #[test]
    fn a_roster_member_outranks_a_directory_stranger() {
        let (directory, roster) = directory();
        let ranked = rank_candidates("mat", &roster, &directory, 10);
        assert_eq!(ranked[0].pubkey, pk("matt"));
        assert!(ranked[0].in_roster);
        assert_eq!(ranked[1].pubkey, pk("stranger"));
        assert!(!ranked[1].in_roster);
    }

    /// A roster **substring** match still outranks a directory **prefix**
    /// match — the person is here, which dominates match quality.
    #[test]
    fn roster_membership_dominates_match_quality() {
        let mut directory = std::collections::BTreeMap::new();
        directory.insert(
            pk("here"),
            Profile {
                pubkey: pk("here"),
                display_name: Some("xxagentxx".into()),
                ..Default::default()
            },
        );
        directory.insert(
            pk("away"),
            Profile {
                pubkey: pk("away"),
                display_name: Some("agentic".into()),
                ..Default::default()
            },
        );
        let roster = [pk("here")].into_iter().collect();
        let ranked = rank_candidates("agent", &roster, &directory, 10);
        assert_eq!(ranked[0].pubkey, pk("here"));
    }

    /// A prefix match beats a substring match within the same roster class.
    #[test]
    fn a_prefix_match_beats_a_substring_match() {
        let mut directory = std::collections::BTreeMap::new();
        for (tag, label) in [("pre", "annabel"), ("sub", "joanna")] {
            directory.insert(
                pk(tag),
                Profile {
                    pubkey: pk(tag),
                    display_name: Some(label.into()),
                    ..Default::default()
                },
            );
        }
        let roster = [pk("pre"), pk("sub")].into_iter().collect();
        let ranked = rank_candidates("anna", &roster, &directory, 10);
        assert_eq!(ranked[0].pubkey, pk("pre"));
    }

    /// The order must be **total**: a picker whose rows permute between
    /// keystrokes moves the selection out from under the operator's fingers.
    #[test]
    fn the_ranking_is_deterministic() {
        let (directory, roster) = directory();
        let first: Vec<String> = rank_candidates("ma", &roster, &directory, 10)
            .into_iter()
            .map(|c| c.pubkey)
            .collect();
        let second: Vec<String> = rank_candidates("ma", &roster, &directory, 10)
            .into_iter()
            .map(|c| c.pubkey)
            .collect();
        assert_eq!(first, second);
    }

    /// The picker opens on `@` with nothing typed. An empty list there reads
    /// as "no one to mention" rather than "keep typing", so an empty prefix
    /// lists the roster.
    #[test]
    fn an_empty_prefix_lists_the_roster_only() {
        let (directory, roster) = directory();
        let ranked = rank_candidates("", &roster, &directory, 10);
        assert_eq!(ranked.len(), 3);
        assert!(ranked.iter().all(|c| c.in_roster));
    }

    /// Matching is case-insensitive, because nobody types the capital.
    #[test]
    fn matching_ignores_case() {
        let (directory, roster) = directory();
        assert_eq!(
            rank_candidates("MATT", &roster, &directory, 10)[0].pubkey,
            pk("matt")
        );
    }

    /// Agents are candidates too — §4.1's scope is "@-mentions with
    /// autocomplete (humans **and agents**)" — and are flagged so the picker
    /// can mark them.
    #[test]
    fn agents_are_candidates_and_are_flagged() {
        let (directory, roster) = directory();
        let ranked = rank_candidates("claude", &roster, &directory, 10);
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].is_agent);
    }

    /// `limit` is honoured, so the picker cannot be handed a thousand rows.
    #[test]
    fn the_limit_is_honoured() {
        let (directory, roster) = directory();
        assert_eq!(rank_candidates("", &roster, &directory, 2).len(), 2);
    }

    /// A profile with no name is still selectable — falling back to the pubkey
    /// prefix keeps two unnamed identities distinguishable, which "unknown"
    /// would not.
    #[test]
    fn an_unnamed_profile_falls_back_to_its_pubkey_prefix() {
        let profile = Profile {
            pubkey: "abcdef1234".into(),
            name: None,
            display_name: Some(String::new()),
            is_agent: false,
        };
        assert_eq!(profile.label(), "abcdef12");
    }

    /// `display_name` wins over `name`: it is the field a human chose.
    #[test]
    fn display_name_wins_over_name() {
        let profile = Profile {
            pubkey: pk("x"),
            name: Some("handle".into()),
            display_name: Some("Real Name".into()),
            is_agent: false,
        };
        assert_eq!(profile.label(), "Real Name");
    }

    // ── [D-2] the server-side resolution path ─────────────────────────────

    /// The `curl` path delegates to the SDK's own matcher, which handles
    /// multi-word display names longest-first. Reimplementing it is exactly
    /// the divergence [D-2] exists to prevent.
    #[test]
    fn server_side_resolution_handles_multi_word_names() {
        let (directory, roster) = directory();
        let resolved = resolve_from_text("hey @Matt Rickard can you look", &directory, &roster);
        assert_eq!(resolved, vec![pk("matt")]);
    }

    /// A name that is not in the roster does not resolve — the roster is the
    /// authority for who is mentionable in this channel.
    #[test]
    fn a_non_member_name_does_not_resolve() {
        let (directory, roster) = directory();
        assert!(resolve_from_text("hi @Matthias", &directory, &roster).is_empty());
    }

    /// Duplicates collapse: `@matt @matt` is one `p` tag, not two.
    #[test]
    fn repeated_mentions_produce_one_tag() {
        let (directory, roster) = directory();
        let resolved = resolve_from_text("@Margaret and @Margaret", &directory, &roster);
        assert_eq!(resolved.len(), 1);
    }

    // ── The inbox filter ──────────────────────────────────────────────────

    /// §2.4's invariant, plus the conversational scope: a job-lifecycle event
    /// that happens to carry a `p` tag is not a mention, and listing it trains
    /// the operator to ignore the inbox.
    #[test]
    fn the_inbox_filter_is_conversational_and_p_scoped() {
        let me = pk("me");
        let filter = build_inbox_filter(&me, Some(1_700_000_000), 50);
        crate::search::assert_explicit_kinds(&filter, "mention inbox").unwrap();
        assert_eq!(filter["#p"], serde_json::json!([me]));
        assert_eq!(
            filter["kinds"],
            serde_json::json!(crate::timeline::CONVERSATIONAL_KINDS)
        );
        assert_eq!(filter["since"], serde_json::json!(1_700_000_000));
        for kind in [40099u32, 43001, 48100] {
            assert!(
                !crate::timeline::CONVERSATIONAL_KINDS.contains(&kind),
                "{kind} must not reach the mention inbox"
            );
        }
    }
}
