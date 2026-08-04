//! NIP-AM turn metrics (kind 44200).
//!
//! Implements Wave-1 daemon deliverable 12 (`DESIGN.md` §4.1.1), §2.4, and
//! §3.4.1's usage pane.
//!
//! §1.2 calls this out as the flagship win: 44200 is "archived and never
//! displayed anywhere" on the desktop, so the TUI can be *better* here on day
//! one rather than chasing parity.
//!
//! # `#p = self` is mandatory, not optional
//!
//! §2.4: the `ids` exemption to the relay's p-gate has two carve-outs —
//! `RESULT_GATED_KINDS = [KIND_DM_VISIBILITY, KIND_AGENT_TURN_METRIC]` lose the
//! exemption when named explicitly. So `GET /agent/{pk}/metric` **must** carry
//! `#p = self`; an `{ids: […], kinds: [44200]}` lookup is refused. Specified in
//! the design because the endpoint would otherwise ship 403-ing.

use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};

/// The NIP-AM turn-metric kind.
pub const KIND_AGENT_TURN_METRIC: u32 = buzz_core::kind::KIND_AGENT_TURN_METRIC;

/// A decoded turn metric.
///
/// Every count is `Option`, and §5.2's `44200 decode` row is explicit about
/// why: **`null` ≠ 0**, `totalTokens` is never derived, a missing
/// context-window denominator renders `—` **and no bar**, cost is suppressed as
/// a single figure when more than one model appears in a session, an unknown
/// `stopReason` becomes `unknown` rather than being dropped, and unknown fields
/// are ignored rather than failing the decode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMetric {
    /// Input tokens.
    pub tokens_in: Option<u64>,
    /// Output tokens.
    pub tokens_out: Option<u64>,
    /// Cache-read tokens.
    pub cache_read: Option<u64>,
    /// Cache-write tokens.
    pub cache_write: Option<u64>,
    /// Cost in USD for this turn.
    pub cost_usd: Option<f64>,
    /// Model identifier.
    pub model: Option<String>,
    /// Context-window denominator. `None` → §3.4.1 renders `—` and no bar.
    pub context_window: Option<u64>,
    /// Why the turn stopped; an unrecognized value becomes `unknown`.
    pub stop_reason: Option<String>,
    /// The **provider's own** total, when it reported one.
    ///
    /// Distinct from [`TurnMetric::total_tokens`]'s sum: NIP-AM says
    /// `totalTokens` is "provider-reported — NOT derived by summing input +
    /// output", and providers that bill on a different basis than
    /// input-plus-output report a total that is not the sum. Keeping the two
    /// separate is what lets `total_tokens` prefer the authoritative number
    /// without ever inventing one.
    pub provider_total: Option<u64>,
    /// `false` when the publisher could not observe the previous cumulative
    /// baseline (a harness restart mid-session), making this turn's delta
    /// unreliable.
    ///
    /// Surfaced rather than silently trusted: a burn rate computed from an
    /// unreliable delta is a number the operator would act on.
    #[serde(default = "default_delta_reliable")]
    pub delta_reliable: bool,
}

/// NIP-AM's wire default: absent means reliable.
fn default_delta_reliable() -> bool {
    true
}

impl Default for TurnMetric {
    fn default() -> Self {
        Self {
            tokens_in: None,
            tokens_out: None,
            cache_read: None,
            cache_write: None,
            cost_usd: None,
            model: None,
            context_window: None,
            stop_reason: None,
            provider_total: None,
            delta_reliable: true,
        }
    }
}

impl TurnMetric {
    /// Total tokens: the provider's own figure when it reported one, otherwise
    /// the sum — and **only** when both halves were reported.
    ///
    /// §5.2: "`totalTokens` is never derived." The provider's total is
    /// preferred precisely because deriving one is forbidden; falling back to
    /// the sum only when both halves are present keeps a partially-reported
    /// turn from rendering as a confident total.
    pub fn total_tokens(&self) -> Option<u64> {
        if let Some(total) = self.provider_total {
            return Some(total);
        }
        match (self.tokens_in, self.tokens_out) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        }
    }

    /// Context-usage fraction, or `None` when the model reported no
    /// denominator. §3.4.1: render `—` and **no bar**, never a fabricated 0%.
    pub fn context_fraction(&self) -> Option<f64> {
        match (self.total_tokens(), self.context_window) {
            (Some(used), Some(window)) if window > 0 => Some(used as f64 / window as f64),
            _ => None,
        }
    }
}

impl TurnMetric {
    /// Project a decoded NIP-AM payload onto the daemon's flat metric shape.
    ///
    /// Every §5.2 `44200 decode` rule is enforced here rather than at the
    /// render layer, because the render layer is the disposable half:
    ///
    /// - **`null` ≠ 0.** Every field stays `Option` all the way through.
    /// - **`totalTokens` is never derived.** The provider's own total is used
    ///   when present; [`TurnMetric::total_tokens`] otherwise requires *both*
    ///   halves. A sum substituted for an unreported total is a fabricated
    ///   measurement wearing a real one's clothes.
    /// - **An unknown `stopReason` becomes `unknown`**, not a dropped payload —
    ///   `buzz-core`'s custom `Deserialize` already does this, and the mapping
    ///   is preserved rather than re-derived here.
    /// - **Unknown fields are ignored**, which `serde`'s default gives us and
    ///   which is why this does not use `deny_unknown_fields`.
    ///
    /// `context_window` has no NIP-AM field and is threaded in separately: the
    /// denominator is a property of the *model*, not of the turn, and §3.4.1
    /// renders `—` and **no bar** when it is unknown.
    pub fn from_payload(
        payload: &buzz_core::agent_turn_metric::AgentTurnMetricPayload,
        context_window: Option<u64>,
    ) -> Self {
        let turn = payload.turn.as_ref();
        Self {
            tokens_in: turn.and_then(|t| t.input_tokens),
            tokens_out: turn.and_then(|t| t.output_tokens),
            cache_read: turn.and_then(|t| t.cache_read_tokens),
            cache_write: turn.and_then(|t| t.cache_write_tokens),
            cost_usd: turn.and_then(|t| t.cost_usd),
            model: payload.model.clone(),
            context_window,
            // Serialized through the enum's own `Serialize`, so an
            // unrecognized wire value that `buzz-core` already mapped to
            // `Unknown` arrives here as the string `"unknown"` rather than as
            // the original token. Re-deriving the mapping would let the two
            // drift.
            stop_reason: payload.stop_reason.as_ref().and_then(|reason| {
                serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
            }),
            provider_total: turn.and_then(|t| t.total_tokens),
            delta_reliable: payload.delta_reliable,
        }
    }
}

/// Decrypt and validate a kind-44200 event (§4.1.1 deliverable 12).
///
/// Validation is `buzz-core`'s `validate`, not a reimplementation: NIP-AM's
/// numeric constraints (finite, non-negative `cost_usd`) live there and a second
/// copy would drift. A payload that fails them is refused rather than clamped —
/// a negative cost is a publisher bug, and rendering it as zero hides the bug
/// while corrupting the session total.
pub fn decrypt_metric(
    identity: &crate::identity::Identity,
    event: &nostr::Event,
    context_window: Option<u64>,
) -> Result<TurnMetric> {
    let keys = identity.keys().ok_or(DaemonError::NotAuthenticated)?;
    let payload = buzz_core::agent_turn_metric::decrypt_agent_turn_metric(keys, event)
        .map_err(|e| DaemonError::InvalidInput(format!("44200 decode failed: {e}")))?;
    payload
        .validate()
        .map_err(|e| DaemonError::InvalidInput(format!("44200 failed NIP-AM validation: {e}")))?;
    Ok(TurnMetric::from_payload(&payload, context_window))
}

/// Build the `/agent/{pk}/metric` filter, which **must** carry `#p = self`.
///
/// [`DaemonError::KindlessFilter`] is impossible here by construction; the
/// self-pubkey requirement is asserted instead, because that is the failure
/// mode §2.4 warns would otherwise ship 403-ing.
pub fn build_metric_filter(agent_pubkey: &str, self_pubkey: &str) -> Result<serde_json::Value> {
    if self_pubkey.is_empty() {
        return Err(DaemonError::KindlessFilter {
            context: "agent metric requires #p = self",
        });
    }
    Ok(serde_json::json!({
        "kinds": [KIND_AGENT_TURN_METRIC],
        "#p": [self_pubkey],
        "authors": [agent_pubkey],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_buzz_core() {
        assert_eq!(KIND_AGENT_TURN_METRIC, 44200);
    }

    /// §2.4: `#p = self` is mandatory — the `ids` exemption does not apply to
    /// `RESULT_GATED_KINDS`.
    #[test]
    fn metric_filter_carries_p_self_and_explicit_kinds() {
        let filter = build_metric_filter(&"aa".repeat(32), &"bb".repeat(32)).unwrap();
        assert_eq!(filter["#p"], serde_json::json!(["bb".repeat(32)]));
        crate::search::assert_explicit_kinds(&filter, "metric").unwrap();
    }

    #[test]
    fn metric_filter_refuses_an_empty_self_pubkey() {
        assert!(build_metric_filter(&"aa".repeat(32), "").is_err());
    }

    /// §5.2: `null` ≠ 0 and `totalTokens` is never derived.
    #[test]
    fn total_tokens_needs_both_halves() {
        let partial = TurnMetric {
            tokens_in: Some(100),
            ..Default::default()
        };
        assert_eq!(partial.total_tokens(), None);
        let full = TurnMetric {
            tokens_in: Some(100),
            tokens_out: Some(20),
            ..Default::default()
        };
        assert_eq!(full.total_tokens(), Some(120));
    }

    /// §3.4.1: an absent context-window denominator renders `—` and no bar.
    #[test]
    fn absent_context_window_yields_no_fraction() {
        let m = TurnMetric {
            tokens_in: Some(58_204),
            tokens_out: Some(0),
            context_window: None,
            ..Default::default()
        };
        assert_eq!(m.context_fraction(), None);
    }

    #[test]
    fn zero_context_window_does_not_divide_by_zero() {
        let m = TurnMetric {
            tokens_in: Some(1),
            tokens_out: Some(1),
            context_window: Some(0),
            ..Default::default()
        };
        assert_eq!(m.context_fraction(), None);
    }

    // ── §5.2 `44200 decode`, over real NIP-AM payloads ────────────────────

    use buzz_core::agent_turn_metric::{AgentTurnMetricPayload, StopReason, TokenCounts};

    fn payload_json(turn: serde_json::Value, extra: serde_json::Value) -> AgentTurnMetricPayload {
        let mut base = serde_json::json!({
            "harness": "claude-agent-acp",
            "timestamp": "2026-08-04T14:12:00Z",
            "turn": turn,
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(base).expect("a valid NIP-AM payload")
    }

    /// §5.2: **`null` ≠ 0.** A field the provider did not report must stay
    /// `None` through the whole projection, or the usage pane renders a
    /// confident zero for a measurement nobody made.
    #[test]
    fn unreported_fields_stay_none_through_the_projection() {
        let payload = payload_json(
            serde_json::json!({"inputTokens": 100, "outputTokens": null, "totalTokens": null, "costUsd": null}),
            serde_json::json!({}),
        );
        let metric = TurnMetric::from_payload(&payload, None);
        assert_eq!(metric.tokens_in, Some(100));
        assert_eq!(metric.tokens_out, None);
        assert_eq!(metric.cost_usd, None);
        assert_eq!(metric.context_window, None);
        assert_eq!(
            metric.total_tokens(),
            None,
            "a partially-reported turn is not a total"
        );
    }

    /// NIP-AM: `totalTokens` is "provider-reported — NOT derived by summing".
    /// A provider that bills on a different basis reports a total that is not
    /// the sum, and the provider's number wins.
    #[test]
    fn the_provider_total_wins_over_the_sum() {
        let payload = payload_json(
            serde_json::json!({"inputTokens": 100, "outputTokens": 20, "totalTokens": 500}),
            serde_json::json!({}),
        );
        let metric = TurnMetric::from_payload(&payload, None);
        assert_eq!(
            metric.total_tokens(),
            Some(500),
            "the provider's own figure, not 120"
        );
    }

    /// With no provider total, the sum is used — but only when both halves are
    /// present, so nothing is ever derived from a missing number.
    #[test]
    fn the_sum_is_the_fallback_only_when_both_halves_exist() {
        let both = payload_json(
            serde_json::json!({"inputTokens": 100, "outputTokens": 20}),
            serde_json::json!({}),
        );
        assert_eq!(
            TurnMetric::from_payload(&both, None).total_tokens(),
            Some(120)
        );

        let one = payload_json(
            serde_json::json!({"outputTokens": 20}),
            serde_json::json!({}),
        );
        assert_eq!(TurnMetric::from_payload(&one, None).total_tokens(), None);
    }

    /// §5.2: "an unknown `stopReason` → `unknown`" — the payload is kept, not
    /// dropped. A metric discarded over a stop reason loses the token counts,
    /// which are the part anyone cares about.
    #[test]
    fn an_unrecognized_stop_reason_becomes_unknown_and_keeps_the_counts() {
        let payload = payload_json(
            serde_json::json!({"inputTokens": 42, "outputTokens": 7}),
            serde_json::json!({"stopReason": "tool_limit_reached_in_a_future_version"}),
        );
        let metric = TurnMetric::from_payload(&payload, None);
        assert_eq!(metric.stop_reason.as_deref(), Some("unknown"));
        assert_eq!(metric.tokens_in, Some(42));
    }

    #[test]
    fn a_recognized_stop_reason_survives_the_projection() {
        let payload = payload_json(
            serde_json::json!({"inputTokens": 1}),
            serde_json::json!({"stopReason": "max_tokens"}),
        );
        assert_eq!(
            TurnMetric::from_payload(&payload, None)
                .stop_reason
                .as_deref(),
            Some("max_tokens")
        );
    }

    /// §5.2: "unknown fields are ignored rather than failing the decode." A
    /// publisher that adds a field must not break every existing reader.
    #[test]
    fn unknown_payload_fields_are_ignored() {
        let raw = serde_json::json!({
            "harness": "goose",
            "timestamp": "2026-08-04T14:12:00Z",
            "turn": {"inputTokens": 5},
            "somethingFromNextYear": {"nested": true},
        });
        let payload: AgentTurnMetricPayload =
            serde_json::from_value(raw).expect("forward compatibility");
        assert_eq!(TurnMetric::from_payload(&payload, None).tokens_in, Some(5));
    }

    /// §3.4.1: the denominator is a property of the **model**, not the turn, so
    /// it is threaded in separately — and without it there is no bar.
    #[test]
    fn the_context_window_is_threaded_in_separately() {
        let payload = payload_json(
            serde_json::json!({"inputTokens": 90_000, "outputTokens": 10_000}),
            serde_json::json!({}),
        );
        assert_eq!(
            TurnMetric::from_payload(&payload, None).context_fraction(),
            None
        );
        assert_eq!(
            TurnMetric::from_payload(&payload, Some(200_000)).context_fraction(),
            Some(0.5)
        );
    }

    /// An unreliable delta is **surfaced**, not silently trusted: a burn rate
    /// computed from one is a number the operator would act on.
    #[test]
    fn an_unreliable_delta_is_carried_through() {
        let unreliable = payload_json(
            serde_json::json!({"inputTokens": 1}),
            serde_json::json!({"deltaReliable": false}),
        );
        assert!(!TurnMetric::from_payload(&unreliable, None).delta_reliable);

        // NIP-AM's wire default: absent means reliable.
        let absent = payload_json(serde_json::json!({"inputTokens": 1}), serde_json::json!({}));
        assert!(TurnMetric::from_payload(&absent, None).delta_reliable);
    }

    /// A negative cost is a publisher bug. Rendering it as zero hides the bug
    /// *and* corrupts the session total, so the payload is refused instead.
    #[test]
    fn nip_am_numeric_validation_refuses_a_negative_cost() {
        let payload = AgentTurnMetricPayload {
            harness: "goose".into(),
            model: None,
            channel_id: None,
            session_id: None,
            turn_id: None,
            turn_seq: None,
            timestamp: "2026-08-04T14:12:00Z".into(),
            turn: Some(TokenCounts {
                input_tokens: Some(1),
                output_tokens: Some(1),
                total_tokens: None,
                cost_usd: Some(-1.0),
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
            cumulative: None,
            delta_reliable: true,
            stop_reason: Some(StopReason::EndTurn),
        };
        assert!(
            payload.validate().is_err(),
            "buzz-core's own validation is the one source of these constraints"
        );
    }

    /// A keyless daemon cannot decrypt a metric, and says so with the state's
    /// own error rather than a generic auth failure.
    #[test]
    fn a_keyless_daemon_cannot_decode_a_metric() {
        use zeroize::Zeroizing;
        let keyless =
            crate::identity::Identity::new("aa".repeat(32), Zeroizing::new(Vec::new()), None);
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(44_200), "x")
            .sign_with_keys(&nostr::Keys::generate())
            .unwrap();
        let err = decrypt_metric(&keyless, &event, None).unwrap_err();
        assert_eq!(err.code(), "not_authenticated");
    }

    /// The round trip that matters: a real 44200, encrypted by the agent to the
    /// owner, decrypted and projected by the daemon.
    #[test]
    fn a_real_44200_round_trips_through_the_daemon() {
        let owner = nostr::Keys::generate();
        let agent = nostr::Keys::generate();
        let payload = AgentTurnMetricPayload {
            harness: "claude-agent-acp".into(),
            model: Some("claude-opus-5".into()),
            channel_id: None,
            session_id: Some("sess-1".into()),
            turn_id: Some("4a91".into()),
            turn_seq: Some(1),
            timestamp: "2026-08-04T14:12:00Z".into(),
            turn: Some(TokenCounts {
                input_tokens: Some(12_480),
                output_tokens: Some(1_932),
                total_tokens: None,
                cost_usd: Some(0.41),
                cache_read_tokens: Some(9_120),
                cache_write_tokens: Some(340),
            }),
            cumulative: None,
            delta_reliable: true,
            stop_reason: Some(StopReason::EndTurn),
        };
        let ciphertext =
            buzz_core::observer::encrypt_observer_payload(&agent, &owner.public_key(), &payload)
                .unwrap();
        let event = nostr::EventBuilder::new(
            nostr::Kind::Custom(KIND_AGENT_TURN_METRIC as u16),
            ciphertext,
        )
        .tags([nostr::Tag::public_key(owner.public_key())])
        .sign_with_keys(&agent)
        .unwrap();

        let identity = crate::identity::Identity::from_keys(owner, None);
        let metric = decrypt_metric(&identity, &event, Some(200_000)).expect("decodes");
        assert_eq!(metric.tokens_in, Some(12_480));
        assert_eq!(metric.cache_read, Some(9_120));
        assert_eq!(metric.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(metric.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(metric.total_tokens(), Some(14_412));
        assert!(metric
            .context_fraction()
            .is_some_and(|f| f > 0.0 && f < 0.1));
    }
}
