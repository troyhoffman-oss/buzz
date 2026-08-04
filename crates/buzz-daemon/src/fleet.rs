//! Fleet reduction — `GET /agent/fleet`.
//!
//! Implements Wave-1 daemon deliverable 10 (`DESIGN.md` §4.1.1) and §3.4.
//!
//! The fleet view is the triage surface: without it the loop is "TUI for the
//! agent I'm already watching, desktop for figuring out *which* agent to
//! watch" — which keeps the desktop open, which keeps it primary, which means
//! the five-day exit criterion (§4.1.4) gets negotiated rather than met.
//!
//! It is also strictly *less* work than the transcript: a reduction over data
//! the daemon already folds, and it degrades to XS beautifully because it is a
//! table of short numbers.

use serde::{Deserialize, Serialize};

/// Agent state class, in the **fixed** sort order of §3.4.
///
/// "Sorted blocked-first, always. Blocked ▸ working ▸ idle ▸ offline ▸ unknown;
/// within a class, longest-waiting first. **Sort order is not a preference.**"
/// The `Ord` derive below encodes that: variant order *is* the sort order, so a
/// preference cannot be threaded through without changing this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Awaiting an answer. Always first.
    Blocked,
    /// A turn is in flight.
    Working,
    /// Present, no turn.
    Idle,
    /// Known to be gone.
    Offline,
    /// **No presence beat seen since this daemon started and no 40902 snapshot
    /// yet** (§2.4). Never collapsed into [`AgentState::Offline`] — rendering
    /// `offline` for "I just started and have not heard anything yet" is
    /// exactly the looks-idle-while-the-socket-is-dead failure §1.3 property 3
    /// forbids.
    Unknown,
}

/// One row of the fleet table (§3.4).
///
/// Every numeric field is `Option`: **`—` means not reported, never `0`** — the
/// §3.4.1 null rule applies here too. A `0` where the agent reported nothing is
/// a fabricated measurement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetRow {
    /// Agent pubkey, lowercase hex.
    pub pubkey: String,
    /// Display name.
    pub name: String,
    /// State class, driving the sort.
    pub state: AgentState,
    /// Current turn id, when one is in flight.
    pub turn: Option<String>,
    /// Seconds elapsed in the current turn.
    pub elapsed_secs: Option<u64>,
    /// Input tokens this turn.
    pub tokens_in: Option<u64>,
    /// Output tokens this turn.
    pub tokens_out: Option<u64>,
    /// Cost this session. Suppressed as a single figure when more than one
    /// model was used (§5.2's `44200 decode` row).
    pub cost_usd: Option<f64>,
    /// Context-window usage percentage. `None` when the model reports no
    /// denominator — §3.4.1 renders `—` and **no bar** rather than inventing
    /// one.
    pub context_pct: Option<f64>,
    /// **Burn rate**, not a total. "Is this agent stuck burning money" is the
    /// actual cost question and no cumulative figure answers it (§3.4).
    pub tokens_per_min: Option<f64>,
    /// The loop detector: a repeated-identical-tool-call counter over frames
    /// the daemon already folds. §3.4 calls it "roughly twenty lines and the
    /// single highest-value cost signal in the product: totals tell you what
    /// you spent, the loop counter tells you what you are *about* to spend."
    pub repeated_tool_calls: Option<u32>,
}

/// Sort a fleet, blocked-first (§3.4).
///
/// Within a class, longest-waiting first — so a larger `elapsed_secs` sorts
/// earlier. Name and pubkey break the remaining ties, which is what makes the
/// order **total**: §5.2 asks for "blocked-first ordering is stable", and an
/// order that is merely *sorted* still permutes two idle agents run to run,
/// making the fleet view jitter under a live refresh.
pub fn sort_fleet(rows: &mut [FleetRow]) {
    rows.sort_by(|a, b| {
        a.state
            .cmp(&b.state)
            .then_with(|| {
                b.elapsed_secs
                    .unwrap_or(0)
                    .cmp(&a.elapsed_secs.unwrap_or(0))
            })
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.pubkey.cmp(&b.pubkey))
    });
}

/// How many identical consecutive tool calls constitute a loop (§3.4).
///
/// Three is the threshold because two is a legitimate retry — a shell command
/// re-run after an edit, a read after a write. Three identical calls with no
/// intervening different call is the signature of an agent that has stopped
/// making progress.
pub const REPEATED_TOOL_CALL_THRESHOLD: u32 = 3;

/// Per-agent accumulator the fleet reduces over.
///
/// Folded from the observer frames the daemon already decrypts and the 44200
/// metrics it already decodes — §3.4 calls the fleet "strictly *less* work than
/// the transcript: a reduction over data the daemon already folds."
#[derive(Debug, Default, Clone)]
pub struct AgentAccumulator {
    /// Display name.
    pub name: String,
    /// Presence, which is the **only** liveness source for a remote agent
    /// (§3.1: a provider-backed agent's `deployed` status never clears).
    pub presence: Option<crate::presence::Presence>,
    /// Current turn id, when one is in flight.
    pub turn: Option<String>,
    /// Unix seconds the current turn started.
    pub turn_started_at: Option<i64>,
    /// Whether the agent is awaiting an answer to an ask card.
    pub awaiting_answer: bool,
    /// Metrics accumulated this session, newest last.
    pub metrics: Vec<crate::metric::TurnMetric>,
    /// The consecutive-identical-tool-call run, as a `(signature, count)` pair.
    repeat: Option<(String, u32)>,
}

impl AgentAccumulator {
    /// Fold one observer frame in.
    ///
    /// Returns `true` when the frame moved the loop detector past its
    /// threshold, so a caller can raise the signal at the moment it fires
    /// rather than by polling.
    pub fn observe_frame(&mut self, frame: &crate::observer::ObserverFrame) -> bool {
        match frame.kind.as_str() {
            "turn_started" => {
                self.turn = frame.turn_id.clone();
                self.turn_started_at = Some(frame.created_at);
                // A new turn resets the loop detector: repeated calls across a
                // turn boundary are two agents' worth of work, not a loop.
                self.repeat = None;
                false
            }
            "turn_completed" | "turn_ended" | "turn_cancelled" => {
                self.turn = None;
                self.turn_started_at = None;
                self.repeat = None;
                false
            }
            _ => self.observe_tool_call(frame),
        }
    }

    /// The loop detector (§3.4).
    ///
    /// "Roughly twenty lines and the single highest-value cost signal in the
    /// product: totals tell you what you spent, the loop counter tells you what
    /// you are *about* to spend."
    ///
    /// The signature is `(tool name, arguments)`. Arguments are part of it
    /// because `bash` called three times is normal and `bash cargo test` called
    /// three times is not — a name-only signature would fire on every healthy
    /// agent.
    fn observe_tool_call(&mut self, frame: &crate::observer::ObserverFrame) -> bool {
        let Some(signature) = tool_signature(frame) else {
            // A non-tool frame does not break the run. An agent that thinks,
            // then repeats the same call, is still looping; requiring strict
            // adjacency would make the detector silent in practice.
            return false;
        };
        match self.repeat.as_mut() {
            Some((previous, count)) if *previous == signature => {
                *count += 1;
                *count == REPEATED_TOOL_CALL_THRESHOLD
            }
            _ => {
                // A *different* call resets the counter — §5.2: "the
                // repeated-identical-tool-call counter fires at N and resets on
                // a different call."
                self.repeat = Some((signature, 1));
                false
            }
        }
    }

    /// Fold one 44200 turn metric in.
    pub fn observe_metric(&mut self, metric: crate::metric::TurnMetric) {
        self.metrics.push(metric);
    }

    /// The current consecutive-identical-call count, when there is a run.
    pub fn repeated_tool_calls(&self) -> Option<u32> {
        self.repeat
            .as_ref()
            .map(|(_, count)| *count)
            .filter(|count| *count > 1)
    }
}

/// The `(tool, arguments)` signature of a tool-call frame, or `None` when the
/// frame is not one.
fn tool_signature(frame: &crate::observer::ObserverFrame) -> Option<String> {
    let payload = &frame.payload;
    let name = payload
        .get("tool")
        .or_else(|| payload.get("toolName"))
        .or_else(|| payload.get("name"))
        .and_then(serde_json::Value::as_str)?;
    let args = payload
        .get("arguments")
        .or_else(|| payload.get("args"))
        .or_else(|| payload.get("input"))
        .map(ToString::to_string)
        .unwrap_or_default();
    Some(format!("{name}\u{0}{args}"))
}

/// Reduce one agent's accumulator into a fleet row (§4.1.1 deliverable 10).
///
/// `now` is injected rather than read so §5.2's "burn rate over a **frozen
/// clock**" is testable — a rate function that calls `SystemTime::now` cannot
/// be asserted on.
pub fn reduce_agent(pubkey: &str, agent: &AgentAccumulator, now: i64) -> FleetRow {
    let elapsed_secs = agent
        .turn_started_at
        .map(|started| (now - started).max(0) as u64);

    let latest = agent.metrics.last();
    let tokens_in = latest.and_then(|m| m.tokens_in);
    let tokens_out = latest.and_then(|m| m.tokens_out);

    // §5.2 / §3.4.1: "cost suppressed as a single figure when >1 model in
    // session." A sum across models is not a number anyone can act on, and
    // showing one implies a comparability that does not exist.
    let models: std::collections::BTreeSet<&str> = agent
        .metrics
        .iter()
        .filter_map(|m| m.model.as_deref())
        .collect();
    let cost_usd = if models.len() > 1 {
        None
    } else {
        let costs: Vec<f64> = agent.metrics.iter().filter_map(|m| m.cost_usd).collect();
        if costs.is_empty() {
            None
        } else {
            Some(costs.iter().sum())
        }
    };

    FleetRow {
        pubkey: pubkey.to_string(),
        name: agent.name.clone(),
        state: agent_state(agent),
        turn: agent.turn.clone(),
        elapsed_secs,
        tokens_in,
        tokens_out,
        cost_usd,
        context_pct: latest
            .and_then(crate::metric::TurnMetric::context_fraction)
            .map(|f| f * 100.0),
        tokens_per_min: burn_rate(agent, elapsed_secs),
        repeated_tool_calls: agent.repeated_tool_calls(),
    }
}

/// Classify an agent into its sort class.
///
/// Blocked wins over working because an agent awaiting an answer is *also*
/// mid-turn, and the operator's attention belongs on the one that cannot
/// proceed without them.
fn agent_state(agent: &AgentAccumulator) -> AgentState {
    if agent.awaiting_answer {
        return AgentState::Blocked;
    }
    match agent.presence {
        // Presence is the only liveness source for a remote agent (§3.1), so
        // `unknown` presence yields `unknown` state even mid-turn: a turn that
        // started before the daemon did tells you nothing about *now*.
        None | Some(crate::presence::Presence::Unknown) => AgentState::Unknown,
        Some(crate::presence::Presence::Offline) => AgentState::Offline,
        Some(_) if agent.turn.is_some() => AgentState::Working,
        Some(_) => AgentState::Idle,
    }
}

/// Tokens per minute over the current turn (§3.4).
///
/// **A rate, not a total.** "Is this agent stuck burning money" is the actual
/// cost question and no cumulative figure answers it. `None` when either half
/// is missing — a rate computed from a fabricated zero is worse than no rate.
fn burn_rate(agent: &AgentAccumulator, elapsed_secs: Option<u64>) -> Option<f64> {
    let elapsed = elapsed_secs.filter(|s| *s > 0)?;
    let total = agent.metrics.last()?.total_tokens()?;
    Some(total as f64 * 60.0 / elapsed as f64)
}

/// The fleet reduction: every agent, reduced and sorted blocked-first.
#[derive(Debug, Default)]
pub struct Fleet {
    agents: std::collections::BTreeMap<String, AgentAccumulator>,
}

impl Fleet {
    /// An empty fleet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mutable access to one agent's accumulator, creating it if needed.
    pub fn agent_mut(&mut self, pubkey: &str) -> &mut AgentAccumulator {
        self.agents.entry(pubkey.to_string()).or_default()
    }

    /// One agent's accumulator.
    pub fn agent(&self, pubkey: &str) -> Option<&AgentAccumulator> {
        self.agents.get(pubkey)
    }

    /// The fleet table, sorted blocked-first (§3.4).
    pub fn rows(&self, now: i64) -> Vec<FleetRow> {
        let mut rows: Vec<FleetRow> = self
            .agents
            .iter()
            .map(|(pubkey, agent)| reduce_agent(pubkey, agent, now))
            .collect();
        sort_fleet(&mut rows);
        rows
    }

    /// How many agents are being tracked.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether no agents are known.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, state: AgentState, elapsed: Option<u64>) -> FleetRow {
        FleetRow {
            pubkey: "aa".repeat(32),
            name: name.into(),
            state,
            turn: None,
            elapsed_secs: elapsed,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            context_pct: None,
            tokens_per_min: None,
            repeated_tool_calls: None,
        }
    }

    /// §3.4: "Blocked ▸ working ▸ idle ▸ offline ▸ unknown". Encoded in the
    /// variant order so a preference cannot be threaded through.
    #[test]
    fn state_order_is_blocked_first() {
        let mut states = vec![
            AgentState::Unknown,
            AgentState::Working,
            AgentState::Offline,
            AgentState::Blocked,
            AgentState::Idle,
        ];
        states.sort();
        assert_eq!(
            states,
            vec![
                AgentState::Blocked,
                AgentState::Working,
                AgentState::Idle,
                AgentState::Offline,
                AgentState::Unknown,
            ]
        );
    }

    /// §5.2: "blocked-first ordering is stable and not preference-driven."
    #[test]
    fn sort_puts_blocked_first_then_longest_waiting() {
        let mut rows = vec![
            row("idle-1", AgentState::Idle, None),
            row("working-short", AgentState::Working, Some(48)),
            row("blocked-1", AgentState::Blocked, Some(252)),
            row("working-long", AgentState::Working, Some(750)),
        ];
        sort_fleet(&mut rows);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["blocked-1", "working-long", "working-short", "idle-1"]
        );
    }

    /// §2.4/§3.1: `unknown` is never collapsed into `offline`.
    #[test]
    fn unknown_is_not_offline() {
        assert_ne!(AgentState::Unknown, AgentState::Offline);
    }

    /// §3.4: "`—` means not reported, never `0`."
    #[test]
    fn unreported_metrics_are_none_not_zero() {
        let r = row("a", AgentState::Idle, None);
        assert!(r.context_pct.is_none());
        assert!(r.tokens_per_min.is_none());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"context_pct\":null"), "{json}");
    }

    // ── §5.2 `fleet reduction` ────────────────────────────────────────────

    use crate::metric::TurnMetric;
    use crate::observer::ObserverFrame;
    use crate::presence::Presence;

    const NOW: i64 = 1_785_852_720;

    fn frame(kind: &str, payload: serde_json::Value) -> ObserverFrame {
        ObserverFrame {
            agent_pubkey: "aa".repeat(32),
            seq: 1,
            timestamp: String::new(),
            created_at: NOW,
            kind: kind.into(),
            channel_id: None,
            session_id: None,
            turn_id: Some("4a91".into()),
            started_at: None,
            payload,
        }
    }

    fn tool_frame(tool: &str, args: &str) -> ObserverFrame {
        frame(
            "acp_tool_call",
            serde_json::json!({"tool": tool, "args": args}),
        )
    }

    /// §5.2: "the repeated-identical-tool-call counter fires at N and resets on
    /// a different call."
    #[test]
    fn the_loop_detector_fires_at_the_threshold() {
        let mut agent = AgentAccumulator::default();
        assert!(!agent.observe_frame(&tool_frame("bash", "cargo test")));
        assert!(!agent.observe_frame(&tool_frame("bash", "cargo test")));
        assert!(
            agent.observe_frame(&tool_frame("bash", "cargo test")),
            "the third identical call is the signal"
        );
        assert_eq!(
            agent.repeated_tool_calls(),
            Some(REPEATED_TOOL_CALL_THRESHOLD)
        );
    }

    #[test]
    fn a_different_call_resets_the_loop_detector() {
        let mut agent = AgentAccumulator::default();
        agent.observe_frame(&tool_frame("bash", "cargo test"));
        agent.observe_frame(&tool_frame("bash", "cargo test"));
        agent.observe_frame(&tool_frame("read", "src/lib.rs"));
        assert_eq!(agent.repeated_tool_calls(), None, "the run was broken");
        assert!(!agent.observe_frame(&tool_frame("read", "src/lib.rs")));
    }

    /// The signature includes arguments: `bash` three times is normal, `bash
    /// cargo test` three times is not. A name-only signature would fire on
    /// every healthy agent.
    #[test]
    fn the_signature_includes_arguments() {
        let mut agent = AgentAccumulator::default();
        for args in ["cargo test", "cargo build", "cargo clippy"] {
            assert!(!agent.observe_frame(&tool_frame("bash", args)));
        }
        assert_eq!(agent.repeated_tool_calls(), None);
    }

    /// A new turn resets the detector: repeated calls across a turn boundary
    /// are two units of work, not a loop.
    #[test]
    fn a_new_turn_resets_the_loop_detector() {
        let mut agent = AgentAccumulator::default();
        agent.observe_frame(&tool_frame("bash", "cargo test"));
        agent.observe_frame(&tool_frame("bash", "cargo test"));
        agent.observe_frame(&frame("turn_started", serde_json::Value::Null));
        assert_eq!(agent.repeated_tool_calls(), None);
    }

    /// A thinking frame between two identical calls does not break the run —
    /// requiring strict adjacency would make the detector silent in practice.
    #[test]
    fn a_non_tool_frame_does_not_break_the_run() {
        let mut agent = AgentAccumulator::default();
        agent.observe_frame(&tool_frame("bash", "cargo test"));
        agent.observe_frame(&frame("acp_thought", serde_json::json!({"text": "hmm"})));
        agent.observe_frame(&tool_frame("bash", "cargo test"));
        assert!(agent.observe_frame(&tool_frame("bash", "cargo test")));
    }

    /// §5.2: "burn rate over a **frozen clock**." The clock is injected, so the
    /// rate is assertable at all.
    #[test]
    fn burn_rate_is_tokens_per_minute_over_the_current_turn() {
        let mut agent = AgentAccumulator {
            presence: Some(Presence::Present),
            turn: Some("4a91".into()),
            turn_started_at: Some(NOW - 120),
            ..Default::default()
        };
        agent.observe_metric(TurnMetric {
            tokens_in: Some(9_000),
            tokens_out: Some(1_000),
            ..Default::default()
        });

        let row = reduce_agent(&"aa".repeat(32), &agent, NOW);
        assert_eq!(row.elapsed_secs, Some(120));
        assert_eq!(
            row.tokens_per_min,
            Some(5_000.0),
            "10k tokens over 2 minutes"
        );
    }

    /// A rate computed from a fabricated zero is worse than no rate.
    #[test]
    fn burn_rate_is_none_when_either_half_is_missing() {
        let mut agent = AgentAccumulator {
            presence: Some(Presence::Present),
            ..Default::default()
        };

        // No turn, so no elapsed.
        agent.observe_metric(TurnMetric {
            tokens_in: Some(100),
            tokens_out: Some(20),
            ..Default::default()
        });
        assert_eq!(reduce_agent("pk", &agent, NOW).tokens_per_min, None);

        // A turn, but a metric with only one token half — §5.2's "totalTokens
        // is never derived" rule propagates into the rate.
        agent.turn = Some("t".into());
        agent.turn_started_at = Some(NOW - 60);
        agent.metrics.clear();
        agent.observe_metric(TurnMetric {
            tokens_in: Some(100),
            ..Default::default()
        });
        assert_eq!(reduce_agent("pk", &agent, NOW).tokens_per_min, None);
    }

    /// §5.2 / §3.4.1: "cost suppressed as a single figure when >1 model in
    /// session." A cross-model sum implies a comparability that does not exist.
    #[test]
    fn cost_is_suppressed_when_more_than_one_model_appears() {
        let mut agent = AgentAccumulator {
            presence: Some(Presence::Present),
            ..Default::default()
        };
        agent.observe_metric(TurnMetric {
            cost_usd: Some(0.40),
            model: Some("claude-opus-5".into()),
            ..Default::default()
        });
        assert_eq!(reduce_agent("pk", &agent, NOW).cost_usd, Some(0.40));

        agent.observe_metric(TurnMetric {
            cost_usd: Some(0.05),
            model: Some("claude-haiku".into()),
            ..Default::default()
        });
        assert_eq!(
            reduce_agent("pk", &agent, NOW).cost_usd,
            None,
            "two models means no single figure"
        );
    }

    /// Blocked beats working: an agent awaiting an answer is *also* mid-turn,
    /// and the operator's attention belongs on the one that cannot proceed.
    #[test]
    fn awaiting_an_answer_outranks_being_mid_turn() {
        let mut agent = AgentAccumulator {
            presence: Some(Presence::Present),
            turn: Some("4a91".into()),
            ..Default::default()
        };
        assert_eq!(reduce_agent("pk", &agent, NOW).state, AgentState::Working);
        agent.awaiting_answer = true;
        assert_eq!(reduce_agent("pk", &agent, NOW).state, AgentState::Blocked);
    }

    /// §3.1/§2.4: presence is the **only** liveness source for a remote agent,
    /// so unknown presence yields `unknown` even mid-turn — a turn that started
    /// before the daemon did says nothing about now.
    #[test]
    fn unknown_presence_yields_unknown_state_even_mid_turn() {
        let mut agent = AgentAccumulator {
            turn: Some("4a91".into()),
            ..Default::default()
        };
        assert_eq!(reduce_agent("pk", &agent, NOW).state, AgentState::Unknown);
        agent.presence = Some(Presence::Unknown);
        assert_eq!(reduce_agent("pk", &agent, NOW).state, AgentState::Unknown);
        agent.presence = Some(Presence::Offline);
        assert_eq!(reduce_agent("pk", &agent, NOW).state, AgentState::Offline);
    }

    /// §5.2: the order must be **stable**, not merely sorted — two idle agents
    /// permuting run to run makes the fleet view jitter under a live refresh.
    #[test]
    fn the_fleet_order_is_total_and_reproducible() {
        let mut fleet = Fleet::new();
        for (pubkey, name, state) in [
            ("pk-c", "charlie", Presence::Present),
            ("pk-a", "alpha", Presence::Present),
            ("pk-b", "bravo", Presence::Present),
        ] {
            let agent = fleet.agent_mut(pubkey);
            agent.name = name.into();
            agent.presence = Some(state);
        }
        let first: Vec<String> = fleet.rows(NOW).into_iter().map(|r| r.name).collect();
        let second: Vec<String> = fleet.rows(NOW).into_iter().map(|r| r.name).collect();
        assert_eq!(first, ["alpha", "bravo", "charlie"]);
        assert_eq!(first, second);
    }

    /// The whole point of the view: the blocked agent is first regardless of
    /// how the map happens to be keyed.
    #[test]
    fn the_blocked_agent_leads_the_fleet() {
        let mut fleet = Fleet::new();
        let working = fleet.agent_mut("pk-zulu");
        working.name = "zulu".into();
        working.presence = Some(Presence::Present);
        working.turn = Some("t".into());
        working.turn_started_at = Some(NOW - 750);

        let blocked = fleet.agent_mut("pk-alpha");
        blocked.name = "alpha".into();
        blocked.presence = Some(Presence::Present);
        blocked.awaiting_answer = true;

        let rows = fleet.rows(NOW);
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[0].state, AgentState::Blocked);
        assert_eq!(rows[1].state, AgentState::Working);
    }

    /// §3.4.1: an absent context-window denominator renders `—` and no bar; it
    /// must not become 0%.
    #[test]
    fn an_absent_context_window_yields_no_percentage() {
        let mut agent = AgentAccumulator {
            presence: Some(Presence::Present),
            ..Default::default()
        };
        agent.observe_metric(TurnMetric {
            tokens_in: Some(58_204),
            tokens_out: Some(0),
            context_window: None,
            ..Default::default()
        });
        assert_eq!(reduce_agent("pk", &agent, NOW).context_pct, None);

        agent.metrics.clear();
        agent.observe_metric(TurnMetric {
            tokens_in: Some(50_000),
            tokens_out: Some(50_000),
            context_window: Some(200_000),
            ..Default::default()
        });
        assert_eq!(reduce_agent("pk", &agent, NOW).context_pct, Some(50.0));
    }

    /// A turn that started in the future (clock skew between agent and daemon)
    /// must not produce a negative elapsed, which would underflow the `u64`.
    #[test]
    fn a_future_turn_start_clamps_to_zero_elapsed() {
        let agent = AgentAccumulator {
            presence: Some(Presence::Present),
            turn: Some("t".into()),
            turn_started_at: Some(NOW + 30),
            ..Default::default()
        };
        assert_eq!(reduce_agent("pk", &agent, NOW).elapsed_secs, Some(0));
    }
}
