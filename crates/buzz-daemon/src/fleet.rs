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
/// earlier.
pub fn sort_fleet(rows: &mut [FleetRow]) {
    rows.sort_by(|a, b| {
        a.state.cmp(&b.state).then_with(|| {
            b.elapsed_secs
                .unwrap_or(0)
                .cmp(&a.elapsed_secs.unwrap_or(0))
        })
    });
}

/// The fleet reduction.
///
/// TODO(wave1, §4.1.1 deliverable 10): fold per-agent state, current turn,
/// elapsed, in/out tokens, per-model cost, context %, burn rate, and the
/// repeated-identical-tool-call counter into [`FleetRow`]s, sorted by
/// [`sort_fleet`].
#[derive(Debug, Default)]
pub struct Fleet {
    _private: (),
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
}
