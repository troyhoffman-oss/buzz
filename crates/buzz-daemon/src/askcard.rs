//! Ask-card projection and answering.
//!
//! Implements Wave-1 daemon deliverable 9 (`DESIGN.md` §4.1.1), §2.4, and
//! §3.4.1.
//!
//! # Permission answering is not a control frame
//!
//! §2.4 is explicit: it is `POST /message/{id}/ask`. The endpoint publishes a
//! **threaded kind:9 reply** carrying `askReplyContent` / `askReplyMentions`
//! semantics from `desktop/src/features/messages/lib/askCard.ts`, **including
//! the `broadcast` tag** — a thread-only reply never reaches the channel window
//! and the card's answered-state derivation breaks without it.
//!
//! This matters because the adjacent mechanism is a control frame, and
//! `POST /agent/{pk}/control` accepts exactly two payloads
//! ([`crate::observer::ControlPayload`]). Routing an answer there would be
//! logged-and-dropped by the harness — silently.

use serde::{Deserialize, Serialize};

/// Maximum options on a valid ask card.
///
/// Origin: `desktop/src/features/messages/lib/askCard.ts:31`
/// (`ASK_MAX_OPTIONS`). More than this parses to `null`, i.e. "not an ask
/// card", not "an ask card with too many options".
pub const ASK_MAX_OPTIONS: usize = 20;

/// Supported ask-card schema version. `v: 2` parses to `null` (§5.2).
pub const ASK_VERSION: u32 = 1;

/// Tag name that must ride on an ask answer.
///
/// Without it a thread-only reply never reaches the channel window and the
/// card's answered-state derivation breaks (§2.4).
pub const BROADCAST_TAG: &str = "broadcast";

/// Tag name carrying the structured question.
///
/// Origin: `desktop/src/features/messages/lib/askCard.ts:20` (`ASK_TAG_NAME`).
pub const ASK_TAG_NAME: &str = "ask";

/// A parsed ask card, projected onto the hydrated message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskCard {
    /// The question text. Empty is invalid.
    pub question: String,
    /// At most [`ASK_MAX_OPTIONS`] options.
    pub options: Vec<AskOption>,
    /// `true` when the form field is an array — several labels, comma-joined.
    pub multi_select: bool,
    /// `true` when an answer naming no option is still accepted.
    ///
    /// A question with **no options is free-text whatever the payload claims**:
    /// a card offering neither buttons nor a text box is unanswerable, and
    /// rendering one is worse than rendering the numbered body.
    pub allow_free_text: bool,
    /// 0-based position within a multi-question form.
    pub index: u32,
    /// How many questions the form has.
    pub total: u32,
    /// Routing: [`AskRouting::AskOwner`] is actionable,
    /// [`AskRouting::Auto`] is informational and already answered with no
    /// affordance (§5.3's `agent-ask` fixture covers both).
    pub routing: AskRouting,
}

/// One selectable answer.
///
/// Field names are **verbatim from the wire** (`askCard.ts:33`): `label` plus an
/// optional `description`. An earlier scaffold shape carried `value`/`label`,
/// which no producer emits — the answer is a **1-based index**, not a value
/// string, so there is no `value` on the wire to carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    /// Human label rendered in the card.
    pub label: String,
    /// Optional detail line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// How the ask was routed, which decides whether the card is actionable.
///
/// Mirrors the harness's `PermissionRouting` (`crates/buzz-acp/src/acp.rs:283`).
/// **The precondition is usually absent** (§3.4.1): the harness default is
/// `PermissionRouting::Auto`, i.e. permissions are auto-approved and never reach
/// the owner at all unless the agent was deployed with
/// `--permission-mode askOwner`.
///
/// The wire names are `snake_case` because they are daemon→TUI vocabulary, not
/// a relay contract — the harness's `askOwner` spelling is a *clap* alias on
/// `PermissionMode`, whose own wire string to the agent is `default`
/// (`crates/buzz-acp/src/config.rs:155`). Carrying that spelling here would
/// imply a wire compatibility that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskRouting {
    /// The owner must answer; the turn blocks meanwhile. Actionable — `⏎`
    /// publishes the threaded reply.
    AskOwner,
    /// Auto-approved with the request's own `allow_once` option, unattended.
    /// Renders **informational and already-answered**, with no affordance:
    /// "an actionable-looking control that cannot act is worse than no
    /// control" (§3.4.1). **No key answers it** (§5.5).
    Auto,
}

/// Parse the `["ask", json]` tag off an event's tags.
///
/// `askCard.ts`'s `parseAskTag` **verbatim**, including its refusal semantics:
/// anything malformed yields `None` rather than a half-built card, because the
/// message body below the card is always a complete, answerable numbered list.
/// A card is an *enhancement*; a broken card is a regression.
///
/// [`ASK_MAX_OPTIONS`] is a security bound, not a formatting one: any agent key
/// can sign an `ask` tag, and the relay caps kind:9 *content* without capping
/// tags. Over the bound the card is refused and the numbered body renders.
pub fn parse_ask_tag(tags: &[Vec<String>], routing: AskRouting) -> Option<AskCard> {
    let raw = tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some(ASK_TAG_NAME))?
        .get(1)?;
    let payload: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = payload.as_object()?;

    if object.get("v").and_then(serde_json::Value::as_u64) != Some(u64::from(ASK_VERSION)) {
        return None;
    }
    let question = object.get("question").and_then(serde_json::Value::as_str)?;
    if question.is_empty() {
        return None;
    }
    let raw_options = object
        .get("options")
        .and_then(serde_json::Value::as_array)?;
    if raw_options.len() > ASK_MAX_OPTIONS {
        return None;
    }
    let mut options = Vec::with_capacity(raw_options.len());
    for value in raw_options {
        // One malformed option refuses the **whole** card. A card that silently
        // drops an option renumbers every option after it, and the answer is a
        // 1-based index — so a dropped option answers the wrong question.
        options.push(parse_option(value)?);
    }

    let flag = |name: &str| object.get(name).and_then(serde_json::Value::as_bool) == Some(true);
    let count = |name: &str, default: u32| {
        object
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .map_or(default, |n| n as u32)
    };
    Some(AskCard {
        question: question.to_string(),
        allow_free_text: flag("allowFreeText") || options.is_empty(),
        multi_select: flag("multiSelect"),
        options,
        index: count("index", 0),
        total: count("total", 1),
        routing,
    })
}

fn parse_option(value: &serde_json::Value) -> Option<AskOption> {
    let object = value.as_object()?;
    let label = object.get("label").and_then(serde_json::Value::as_str)?;
    if label.is_empty() {
        return None;
    }
    Some(AskOption {
        label: label.to_string(),
        description: object
            .get("description")
            .and_then(serde_json::Value::as_str)
            .filter(|d| !d.is_empty())
            .map(str::to_string),
    })
}

/// The reply body for a set of chosen option indices.
///
/// `askReplyContent` verbatim (`askCard.ts:226`): **1-based indices, comma
/// joined**, never labels. The reason is stated at the source and is worth
/// repeating because the label form looks more readable and is wrong: the
/// harness splits a multi-select answer on `,` before resolving each token, so
/// a label containing a comma ("Yes, immediately") splits into tokens matching
/// no option, and the agent silently receives free strings instead of the
/// option's wire value. `ElicitationField::select` resolves a 1-based index
/// first, so numbers are unambiguous whatever the labels contain.
///
/// `saturating_add` rather than `+`: `indices` arrives from a client request
/// body, and `usize::MAX` overflows. In a debug build that panics — dropping
/// the connection with no error body, because no `CatchPanicLayer` is mounted —
/// and in **release** it silently wraps to `"0"`, which is worse: the agent
/// receives a token matching no option and the operator sees an answer that
/// went nowhere. Saturating keeps the value out of range, where
/// [`check_answerable_indices`] rejects it as the input error it is.
pub fn ask_reply_content(indices: &[usize]) -> String {
    indices
        .iter()
        .map(|index| index.saturating_add(1).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reject an answer whose indices do not name options on this card.
///
/// The card is in hand at the call site, so "is 999 a real option" is a
/// question with an answer — and an unchecked index is not a harmless no-op:
/// `ask_reply_content` is **positional**, so an out-of-range one sends the
/// agent a free-text token matching nothing, and the turn stays blocked while
/// the operator believes they answered.
///
/// An empty selection is refused for the same reason rather than sent as an
/// empty string: a card with options is answered by choosing one.
/// `allow_free_text` cards are the exception — they carry no options to index,
/// so they are answered through the message path, not this one.
pub fn check_answerable_indices(card: &AskCard, indices: &[usize]) -> Result<(), String> {
    if indices.is_empty() {
        return Err("an answer must name at least one option".into());
    }
    if !card.multi_select && indices.len() > 1 {
        return Err(format!(
            "this card is single-select; {} options were chosen",
            indices.len()
        ));
    }
    let options = card.options.len();
    if let Some(bad) = indices.iter().find(|index| **index >= options) {
        return Err(format!(
            "option index {bad} is out of range; this card has {options} options"
        ));
    }
    Ok(())
}

/// The pubkeys an answer must `p`-tag: **the agent that asked, and only it**.
///
/// `askReplyMentions` verbatim (`askCard.ts:240`). The harness subscribes with
/// `#p = [agent_pubkey]`, so an answer carrying no `p` tag is accepted and
/// stored by the relay — the card collapses, the owner sees success — but is
/// never delivered over the agent's REQ, and the question hangs until it is
/// cancelled. The signer is the trust anchor the card was gated on, so it is
/// also the right recipient.
pub fn ask_reply_mentions(signer_pubkey: &str) -> Vec<String> {
    vec![signer_pubkey.to_string()]
}

/// Resolve an answer body back to option labels, for the answered row.
///
/// `askAnswerLabels` verbatim: 1-based index first, then a case-insensitive
/// label match, and a token matching neither is free text shown as typed —
/// which is also what the agent received.
pub fn ask_answer_labels(card: &AskCard, content: &str) -> String {
    let tokens: Vec<&str> = if card.multi_select {
        content.split(',').map(str::trim).collect()
    } else {
        vec![content.trim()]
    };
    tokens
        .into_iter()
        .map(|token| {
            if let Ok(index) = token.parse::<usize>() {
                if index >= 1 && index <= card.options.len() {
                    return card.options[index - 1].label.clone();
                }
            }
            card.options
                .iter()
                .find(|option| option.label.eq_ignore_ascii_case(token))
                .map(|option| option.label.clone())
                .unwrap_or_else(|| token.to_string())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether an ask card can be answered at all (§3.4.1).
///
/// [`AskRouting::Auto`] renders **informational and already-answered, with no
/// affordance**: "an actionable-looking control that cannot act is worse than
/// no control". §5.5 is explicit that **no key answers it**.
pub fn is_answerable(card: &AskCard) -> bool {
    card.routing == AskRouting::AskOwner
}

/// The tags a threaded ask answer must carry (§2.4, §3.4.1).
///
/// Three, and each one is load-bearing:
///
/// - `["h", channel]` — the channel scope every Buzz message carries.
/// - `["e", root, "", "root"]` — the NIP-10 thread reference, so the answer
///   lands under the question.
/// - `["p", agent]` — see [`ask_reply_mentions`]; without it the agent never
///   receives the answer.
/// - **`["broadcast", "1"]`** — §2.4 calls this out specifically: "a thread-only
///   reply never reaches the channel window and the card's answered-state
///   derivation breaks without it." The answer would exist, the agent would get
///   it, and the card would stay open forever.
pub fn ask_answer_tags(
    channel_id: &str,
    root_event_id: &str,
    agent_pubkey: &str,
) -> Vec<Vec<String>> {
    vec![
        vec!["h".into(), channel_id.into()],
        vec![
            "e".into(),
            root_event_id.into(),
            String::new(),
            "root".into(),
        ],
        vec!["p".into(), agent_pubkey.into()],
        vec![BROADCAST_TAG.into(), "1".into()],
    ]
}

/// Open ask cards, and the awaiting count §3.4's fleet view renders.
#[derive(Debug, Default)]
pub struct AskCards {
    /// Open cards by their message event id.
    open: std::collections::BTreeMap<String, (String, AskCard)>,
}

impl AskCards {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an open card asked by `agent_pubkey`.
    ///
    /// Returns `false` when the card is not answerable, so an `Auto` card never
    /// enters the awaiting count — counting it would show a blocked agent that
    /// is not blocked, and §3.4 sorts blocked-first.
    pub fn open(&mut self, event_id: &str, agent_pubkey: &str, card: AskCard) -> bool {
        if !is_answerable(&card) {
            return false;
        }
        self.open
            .insert(event_id.to_string(), (agent_pubkey.to_string(), card));
        true
    }

    /// Close a card once it has been answered.
    pub fn answer(&mut self, event_id: &str) -> Option<AskCard> {
        self.open.remove(event_id).map(|(_, card)| card)
    }

    /// One open card.
    pub fn get(&self, event_id: &str) -> Option<&AskCard> {
        self.open.get(event_id).map(|(_, card)| card)
    }

    /// The agent that asked a still-open question.
    pub fn asker(&self, event_id: &str) -> Option<&str> {
        self.open.get(event_id).map(|(agent, _)| agent.as_str())
    }

    /// How many questions this agent is waiting on — the `blocked` signal
    /// [`crate::fleet::AgentAccumulator::awaiting_answer`] reads.
    pub fn awaiting(&self, agent_pubkey: &str) -> usize {
        self.open
            .values()
            .filter(|(agent, _)| agent == agent_pubkey)
            .count()
    }

    /// Total open cards across every agent.
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Whether nothing is awaiting an answer.
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Origin cited in the module docs.
    #[test]
    fn limits_match_the_desktop() {
        assert_eq!(ASK_MAX_OPTIONS, 20);
        assert_eq!(ASK_VERSION, 1);
    }

    /// §2.4: the answer carries the broadcast tag, or the card never resolves.
    #[test]
    fn broadcast_tag_name_is_fixed() {
        assert_eq!(BROADCAST_TAG, "broadcast");
    }

    /// §3.4.1/§5.5: the two routings are distinct states with distinct wire
    /// names — collapsing them would render an actionable affordance on a card
    /// that cannot act.
    #[test]
    fn routings_are_distinct_and_wire_stable() {
        assert_ne!(AskRouting::AskOwner, AskRouting::Auto);
        assert_eq!(
            serde_json::to_string(&AskRouting::AskOwner).unwrap(),
            "\"ask_owner\""
        );
        assert_eq!(
            serde_json::to_string(&AskRouting::Auto).unwrap(),
            "\"auto\""
        );
    }

    // ── §5.2 `ask card parse` — mirrors askCard.test.mjs ──────────────────

    fn ask_tag(payload: serde_json::Value) -> Vec<Vec<String>> {
        vec![vec![ASK_TAG_NAME.into(), payload.to_string()]]
    }

    fn valid_payload() -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "question": "Which database?",
            "options": [
                {"label": "Postgres", "description": "the one we use"},
                {"label": "SQLite"},
            ],
            "multiSelect": false,
            "allowFreeText": false,
            "index": 0,
            "total": 1,
        })
    }

    #[test]
    fn a_well_formed_tag_parses() {
        let card = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::AskOwner).unwrap();
        assert_eq!(card.question, "Which database?");
        assert_eq!(card.options.len(), 2);
        assert_eq!(card.options[0].label, "Postgres");
        assert_eq!(
            card.options[0].description.as_deref(),
            Some("the one we use")
        );
        assert_eq!(card.options[1].description, None);
        assert!(!card.multi_select);
        assert!(!card.allow_free_text);
    }

    /// §5.2: "malformed json → null."
    #[test]
    fn malformed_json_yields_no_card() {
        let tags = vec![vec![ASK_TAG_NAME.into(), "{not json".into()]];
        assert!(parse_ask_tag(&tags, AskRouting::AskOwner).is_none());
    }

    /// §5.2: "`v: 2` → null." A future schema is refused, not guessed at — the
    /// numbered body below is a complete fallback.
    #[test]
    fn a_future_schema_version_yields_no_card() {
        let mut payload = valid_payload();
        payload["v"] = serde_json::json!(2);
        assert!(parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).is_none());
    }

    /// §5.2: "`options: \"nope\"` → null."
    #[test]
    fn non_array_options_yield_no_card() {
        let mut payload = valid_payload();
        payload["options"] = serde_json::json!("nope");
        assert!(parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).is_none());
    }

    /// §5.2: "`> ASK_MAX_OPTIONS` → null." A security bound: any agent key can
    /// sign an `ask` tag, and the relay caps kind:9 content without capping
    /// tags.
    #[test]
    fn too_many_options_yields_no_card() {
        let mut payload = valid_payload();
        payload["options"] =
            serde_json::json!(vec![serde_json::json!({"label": "x"}); ASK_MAX_OPTIONS + 1]);
        assert!(parse_ask_tag(&ask_tag(payload.clone()), AskRouting::AskOwner).is_none());

        payload["options"] =
            serde_json::json!(vec![serde_json::json!({"label": "x"}); ASK_MAX_OPTIONS]);
        assert!(
            parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).is_some(),
            "exactly at the cap is accepted"
        );
    }

    /// One malformed option refuses the **whole** card: silently dropping it
    /// renumbers every option after it, and the answer is a 1-based index — so
    /// a dropped option answers the wrong question.
    #[test]
    fn one_malformed_option_refuses_the_whole_card() {
        let mut payload = valid_payload();
        payload["options"] = serde_json::json!([{"label": "ok"}, {"description": "no label"}]);
        assert!(parse_ask_tag(&ask_tag(payload.clone()), AskRouting::AskOwner).is_none());

        payload["options"] = serde_json::json!([{"label": ""}]);
        assert!(parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).is_none());
    }

    #[test]
    fn an_empty_question_yields_no_card() {
        let mut payload = valid_payload();
        payload["question"] = serde_json::json!("");
        assert!(parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).is_none());
    }

    #[test]
    fn a_message_with_no_ask_tag_yields_no_card() {
        let tags = vec![vec!["h".to_string(), "chan".to_string()]];
        assert!(parse_ask_tag(&tags, AskRouting::AskOwner).is_none());
    }

    /// A question with **no options is free-text whatever the payload claims**:
    /// a card offering neither buttons nor a text box is unanswerable.
    #[test]
    fn a_card_with_no_options_is_free_text_regardless() {
        let mut payload = valid_payload();
        payload["options"] = serde_json::json!([]);
        payload["allowFreeText"] = serde_json::json!(false);
        let card = parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).unwrap();
        assert!(card.allow_free_text, "an unanswerable card is not a card");
    }

    #[test]
    fn multi_question_position_defaults_to_the_only_question() {
        let mut payload = valid_payload();
        payload.as_object_mut().unwrap().remove("index");
        payload.as_object_mut().unwrap().remove("total");
        let card = parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).unwrap();
        assert_eq!((card.index, card.total), (0, 1));
    }

    // ── The answer ────────────────────────────────────────────────────────

    /// §5.2 / `askReplyContent`: **1-based indices, never labels.** The harness
    /// splits a multi-select answer on `,` before resolving each token, so a
    /// label containing a comma would split into tokens matching no option and
    /// the agent would silently receive free strings.
    #[test]
    fn the_answer_body_is_one_based_indices() {
        assert_eq!(ask_reply_content(&[0]), "1");
        assert_eq!(ask_reply_content(&[0, 2]), "1, 3");
        assert_eq!(ask_reply_content(&[]), "");
    }

    /// The label form would be ambiguous for exactly this option set, which is
    /// why the index form exists.
    #[test]
    fn a_comma_bearing_label_is_why_indices_are_used() {
        let card = AskCard {
            question: "When?".into(),
            options: vec![
                AskOption {
                    label: "Yes, immediately".into(),
                    description: None,
                },
                AskOption {
                    label: "Later".into(),
                    description: None,
                },
            ],
            multi_select: true,
            allow_free_text: false,
            index: 0,
            total: 1,
            routing: AskRouting::AskOwner,
        };
        // The index form round-trips to the right label.
        assert_eq!(
            ask_answer_labels(&card, &ask_reply_content(&[0])),
            "Yes, immediately"
        );
        // Whereas the label form would have split into two unmatched tokens.
        let split: Vec<&str> = "Yes, immediately".split(',').map(str::trim).collect();
        assert_eq!(split, ["Yes", "immediately"]);
    }

    /// §2.4: the answer carries the **broadcast** tag, or the card never
    /// resolves — a thread-only reply never reaches the channel window.
    #[test]
    fn the_answer_carries_every_required_tag() {
        let tags = ask_answer_tags("chan-1", &"ab".repeat(32), &"cd".repeat(32));
        let names: Vec<&str> = tags.iter().map(|t| t[0].as_str()).collect();
        assert!(names.contains(&"h"));
        assert!(names.contains(&"e"));
        assert!(names.contains(&"p"));
        assert!(
            names.contains(&BROADCAST_TAG),
            "without broadcast the card's answered state never derives"
        );
        // The NIP-10 marker must be `root`, or the reply threads under nothing.
        let e_tag = tags.iter().find(|t| t[0] == "e").unwrap();
        assert_eq!(e_tag[3], "root");
    }

    /// `askReplyMentions`: the agent that asked, and **only** it. Without the
    /// `p` tag the answer is stored but never delivered over the agent's REQ,
    /// and the question hangs until it is cancelled.
    #[test]
    fn the_answer_p_tags_exactly_the_asking_agent() {
        let agent = "cd".repeat(32);
        assert_eq!(ask_reply_mentions(&agent), vec![agent]);
    }

    /// The answered row shows labels, so a numbered multi-select reads as
    /// "Postgres, SQLite" rather than "1, 2".
    #[test]
    fn the_answered_row_resolves_indices_back_to_labels() {
        let mut payload = valid_payload();
        payload["multiSelect"] = serde_json::json!(true);
        let card = parse_ask_tag(&ask_tag(payload), AskRouting::AskOwner).unwrap();
        assert_eq!(ask_answer_labels(&card, "1, 2"), "Postgres, SQLite");
    }

    /// A label match is case-insensitive, and a token matching neither an index
    /// nor a label is free text shown as typed — which is what the agent got.
    #[test]
    fn an_unmatched_token_is_shown_as_free_text() {
        let card = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::AskOwner).unwrap();
        assert_eq!(ask_answer_labels(&card, "postgres"), "Postgres");
        assert_eq!(
            ask_answer_labels(&card, "neither, thanks"),
            "neither, thanks"
        );
        assert_eq!(
            ask_answer_labels(&card, "99"),
            "99",
            "an out-of-range index is not an option"
        );
    }

    // ── Routing and the awaiting count ────────────────────────────────────

    /// §3.4.1/§5.5: an `Auto` card is informational and **no key answers it**.
    /// An actionable-looking control that cannot act is worse than no control.
    #[test]
    fn an_auto_routed_card_is_not_answerable() {
        let owner = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::AskOwner).unwrap();
        let auto = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::Auto).unwrap();
        assert!(is_answerable(&owner));
        assert!(!is_answerable(&auto));
    }

    /// An `Auto` card never enters the awaiting count: counting it would show a
    /// blocked agent that is not blocked, and §3.4 sorts blocked-first.
    #[test]
    fn only_answerable_cards_enter_the_awaiting_count() {
        let agent = "cd".repeat(32);
        let mut cards = AskCards::new();

        let auto = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::Auto).unwrap();
        assert!(!cards.open("evt-auto", &agent, auto));
        assert_eq!(cards.awaiting(&agent), 0);

        let owner = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::AskOwner).unwrap();
        assert!(cards.open("evt-owner", &agent, owner));
        assert_eq!(cards.awaiting(&agent), 1);
        assert_eq!(cards.asker("evt-owner"), Some(agent.as_str()));
    }

    /// Answering closes the card, which is what makes the fleet's blocked
    /// signal clear.
    #[test]
    fn answering_clears_the_awaiting_count() {
        let agent = "cd".repeat(32);
        let mut cards = AskCards::new();
        let card = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::AskOwner).unwrap();
        cards.open("evt", &agent, card);
        assert!(cards.answer("evt").is_some());
        assert_eq!(cards.awaiting(&agent), 0);
        assert!(cards.is_empty());
        assert!(
            cards.answer("evt").is_none(),
            "answering twice is not two answers"
        );
    }

    /// The count is per-agent, so one blocked agent does not make the whole
    /// fleet read as blocked.
    #[test]
    fn the_awaiting_count_is_per_agent() {
        let mut cards = AskCards::new();
        let card = parse_ask_tag(&ask_tag(valid_payload()), AskRouting::AskOwner).unwrap();
        cards.open("evt-1", "agent-a", card.clone());
        cards.open("evt-2", "agent-a", card.clone());
        cards.open("evt-3", "agent-b", card);
        assert_eq!(cards.awaiting("agent-a"), 2);
        assert_eq!(cards.awaiting("agent-b"), 1);
        assert_eq!(cards.len(), 3);
    }
}
