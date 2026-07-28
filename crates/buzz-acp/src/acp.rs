//! ACP client module — manages communication with an AI agent subprocess over stdio
//! using JSON-RPC 2.0 (newline-delimited / NDJSON).
//!
//! # Lifecycle
//! 1. [`AcpClient::spawn`] — launch agent binary as subprocess
//! 2. [`AcpClient::initialize`] — protocol version negotiation
//! 3. [`AcpClient::session_new`] — create session with MCP server config
//! 4. [`AcpClient::session_prompt_with_idle_timeout`] — send prompt with idle/hard deadline, return stop reason
//! 5. [`AcpClient::session_cancel`] / [`AcpClient::cancel_with_cleanup`] — cancel in-flight turn

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

use crate::observer::{ObserverContext, ObserverHandle};
use crate::usage::{TurnUsage, UsageTracker};

/// Maximum allowed size of a single NDJSON line from the agent's stdout.
/// Lines exceeding this limit are rejected to prevent OOM from rogue agents.
const MAX_LINE_SIZE: usize = 10_000_000; // 10 MB

/// An MCP server configuration passed to `session/new`.
///
/// Corresponds to the `McpServerStdio` variant in the ACP schema.
/// All four fields are **required** by the schema (`args` and `env` may be empty arrays).
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
}

/// A single environment variable for an MCP server.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// Stop reason returned by `session/prompt` when the agent finishes a turn.
///
/// Maps to the `stopReason` field in the `SessionPromptResponse`.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// Agent completed the turn normally (`"end_turn"`).
    EndTurn,
    /// Turn was cancelled via `session/cancel` (`"cancelled"`).
    Cancelled,
    /// Agent hit its token limit (`"max_tokens"`).
    MaxTokens,
    /// Agent hit its per-turn request limit (`"max_turn_requests"`).
    MaxTurnRequests,
    /// Agent refused the prompt (`"refusal"`).
    /// Note: refused turns are dropped from history by the agent.
    Refusal,
}

impl StopReason {
    /// Parse a `stopReason` string from the ACP wire format.
    ///
    /// Matching is case-insensitive so agents that send `"END_TURN"` or
    /// `"Cancelled"` are handled correctly without a protocol error.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "end_turn" => Some(Self::EndTurn),
            "cancelled" => Some(Self::Cancelled),
            "max_tokens" => Some(Self::MaxTokens),
            "max_turn_requests" => Some(Self::MaxTurnRequests),
            "refusal" => Some(Self::Refusal),
            _ => None,
        }
    }
}

/// Errors that can occur in the ACP client.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Agent process exited unexpectedly")]
    AgentExited,

    #[error("Idle timeout — no agent activity for {0:?}")]
    IdleTimeout(std::time::Duration),

    #[error("Hard turn timeout exceeded (silence {silence:?})")]
    HardTimeout { silence: std::time::Duration },

    #[error("Agent did not stop within {0:?} after cancellation")]
    CancelDrainTimeout(std::time::Duration),

    #[error("Request timeout — agent did not respond within {0:?}")]
    Timeout(std::time::Duration),

    #[error("Write timeout — agent stopped reading stdin (blocked for {0:?})")]
    WriteTimeout(std::time::Duration),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Agent reported error (code {code}): {message}")]
    AgentError { code: i64, message: String },
}

/// Build an [`AcpError::AgentError`] from a JSON-RPC error object,
/// preserving the numeric code. When the `message` field is missing or
/// non-string, fall back to the full JSON object so provider-specific
/// detail (e.g. a `data` field) is not lost.
fn agent_error_from_json(error: &serde_json::Value) -> AcpError {
    let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000);
    let message = match error.get("message").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => error.to_string(),
    };
    AcpError::AgentError { code, message }
}

fn build_initialize_params() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": 2,
        "clientCapabilities": build_client_capabilities(),
        "clientInfo": {
            "name": "buzz-acp",
            "version": env!("CARGO_PKG_VERSION")
        },
    })
}

/// ACP client that owns an agent subprocess and communicates over its stdio.
///
/// One `AcpClient` per agent process. Multiple sessions can be created on the
/// same client via repeated calls to [`session_new`](AcpClient::session_new).
pub struct AcpClient {
    /// The agent child process (kept alive to prevent zombie).
    child: Child,
    /// Write end of the agent's stdin pipe.
    stdin: ChildStdin,
    /// Framed reader over the agent's stdout pipe (line-oriented, bounded).
    /// Uses `LinesCodec::new_with_max_length` to enforce MAX_LINE_SIZE at the
    /// read level — prevents OOM from rogue agents writing infinite non-newline bytes.
    reader: FramedRead<ChildStdout, LinesCodec>,
    /// Monotonically increasing JSON-RPC request id counter.
    /// Harness-generated IDs are always numeric.
    next_id: u64,
    /// The id of a `session/request_permission` request that has been received
    /// but not yet responded to. Stored as `serde_json::Value` because JSON-RPC 2.0
    /// permits both numeric and string IDs from the agent.
    /// Used by [`cancel_with_cleanup`](AcpClient::cancel_with_cleanup) to send
    /// a `cancelled` outcome before the agent returns from `session/prompt`.
    pending_permission_id: Option<serde_json::Value>,
    /// Whether we have already sent a response to the pending permission request.
    /// Guards against double-response if a timeout fires after the allow_once
    /// response was written but before `pending_permission_id` was cleared.
    permission_responded: bool,
    /// The JSON-RPC id of the most recently sent `session/prompt` request.
    /// Used by [`cancel_with_cleanup`] to drain the correct response.
    /// Set in [`session_prompt_with_idle_timeout`]; consumed in [`cancel_with_cleanup`].
    last_prompt_id: Option<u64>,
    /// Hard deadline for the current turn, set by `session_prompt_with_idle_timeout`.
    /// Inherited by `cancel_with_cleanup` so the drain loop shares the same budget
    /// rather than starting a fresh timer (prevents double-jeopardy).
    current_hard_deadline: Option<tokio::time::Instant>,
    /// Optional local observer feed used by the desktop app.
    observer: Option<ObserverHandle>,
    /// Pool slot index for this agent process.
    observer_agent_index: Option<usize>,
    /// Best-effort context attached to raw ACP wire events.
    observer_context: ObserverContext,
    /// Most recently observed `_meta.goose.activeRunId` from a
    /// `session/update` notification of kind `session_info_update`.
    ///
    /// Both goose and buzz-agent emit `session_info_update` with this field;
    /// goose emits it whenever it starts or clears an active prompt run
    /// (`crates/goose/src/acp/server.rs:2277` `send_active_run_update`).
    /// Required as `expectedRunId` when calling the non-standard
    /// `_goose/unstable/session/steer` method to inject a message into an
    /// in-flight turn without cancelling it.
    ///
    /// `None` until the first `session_info_update` arrives, or after the
    /// run clears (goose/buzz-agent emit `activeRunId: null` at end of turn).
    /// Other agents may leave this unset — readers must treat `None` as
    /// "no active run to steer into" and fall back to cancel+merge.
    active_run_id: Option<String>,
    /// Per-turn channel for receiving goose-native non-cancelling steer
    /// requests from the main loop. Installed by
    /// [`install_steer_rx`](Self::install_steer_rx) at dispatch and
    /// consumed (via `take()`) by `session_prompt_with_idle_timeout` so it
    /// is dropped at scope exit alongside the turn it served. `None`
    /// outside of a goose-native turn — the read loop's steer arm is
    /// disabled in that case.
    steer_rx: Option<tokio::sync::mpsc::Receiver<crate::pool::SteerRequest>>,
    /// Per-turn surface an agent question is published to. Installed by
    /// [`install_elicitation`](Self::install_elicitation) at dispatch. `None`
    /// for heartbeat turns and outside a turn — an `elicitation/create` with
    /// nowhere to render is answered `cancel` immediately.
    elicitation: Option<crate::pool::ElicitationAsk>,
    /// Per-turn channel owner replies arrive on. Consumed (via `take()`) by
    /// `read_until_response_with_idle_timeout` so it is dropped at scope exit
    /// alongside the turn it served, exactly like `steer_rx`.
    elicitation_rx: Option<tokio::sync::mpsc::Receiver<crate::pool::ElicitationReply>>,
    /// The `elicitation/create` request awaiting an owner reply, if any. Lives
    /// on the client rather than the read loop, like `pending_permission_id`,
    /// so [`cancel_with_cleanup`](Self::cancel_with_cleanup) can answer it.
    pending_elicitation: Option<PendingElicitation>,
    /// Usage tracker — accumulates cumulative token counts from
    /// `_goose/unstable/session/update` notifications and computes per-turn
    /// deltas. Both goose and buzz-agent emit this notification; goose gates
    /// on client capability advertisement, buzz-agent emits unconditionally.
    goose_usage: UsageTracker,
}

/// Recursively merge `overlay` into `base`, with `overlay` winning on scalar/shape
/// collisions.  When both sides have an object for the same key, the merge recurses so
/// unrelated nested keys from `base` are preserved.
fn deep_merge(
    base: &mut serde_json::Map<String, serde_json::Value>,
    overlay: serde_json::Map<String, serde_json::Value>,
) {
    for (k, overlay_val) in overlay {
        match base.get_mut(&k) {
            Some(serde_json::Value::Object(base_obj))
                if matches!(overlay_val, serde_json::Value::Object(_)) =>
            {
                // Both sides are objects — recurse to preserve unrelated nested keys.
                if let serde_json::Value::Object(overlay_obj) = overlay_val {
                    deep_merge(base_obj, overlay_obj);
                }
            }
            _ => {
                // Scalar, array, type mismatch, or new key — overlay wins.
                base.insert(k, overlay_val);
            }
        }
    }
}

/// Build the merged `CODEX_CONFIG` environment-variable value for a Codex agent spawn.
///
/// Returns `Some(json_string)` when `has_generated_codex_config` is true (Buzz injected a
/// `CODEX_CONFIG` entry via `codex_network_env()`), `None` otherwise.
///
/// # Merge contract (when `has_generated_codex_config` is true)
///
/// 1. **Persona base** — the first `CODEX_CONFIG` value in `extra_env` is taken as
///    the base object (all keys preserved, recursively).  When there is no persona entry,
///    the generated entry serves as the base.
/// 2. **Generated overlay** — all subsequent `CODEX_CONFIG` entries are deep-merged into
///    the base so unrelated nested persona keys survive.
/// 3. **Parent-env precedence** — if `parent_codex_config` is `Some`, its keys are
///    deep-merged into the result (parent wins on colliding keys at every nesting level;
///    unrelated keys from either side survive).
/// 4. **Forced overlay** — `sandbox_workspace_write.network_access = true` is applied
///    last so relay access is guaranteed regardless of operator / persona config.
///
/// When `has_generated_codex_config` is false, the function returns `None` and the
/// caller handles any persona-supplied `CODEX_CONFIG` with ordinary operator-wins
/// semantics (no merging, no sandbox widening).
///
/// # Errors
///
/// Returns `Err(AcpError::Protocol)` when `has_generated_codex_config` is true and any
/// `CODEX_CONFIG` value is not valid JSON or is not a JSON object, or when
/// `sandbox_workspace_write` is present but not an object after all merges.
pub(crate) fn build_codex_config_env(
    extra_env: &[(String, String)],
    parent_codex_config: Option<&str>,
    has_generated_codex_config: bool,
) -> Result<Option<String>, AcpError> {
    // Without an explicit Buzz-generated overlay signal, skip the merge entirely.
    // Any persona CODEX_CONFIG is handled by the caller with operator-wins semantics.
    if !has_generated_codex_config {
        return Ok(None);
    }

    // Collect all CODEX_CONFIG entries from extra_env in order.
    let codex_entries: Vec<&str> = extra_env
        .iter()
        .filter(|(k, _)| k == "CODEX_CONFIG")
        .map(|(_, v)| v.as_str())
        .collect();

    if codex_entries.is_empty() {
        // has_generated_codex_config is true but no entry in extra_env — shouldn't
        // happen in practice, but treat as no-op rather than panic.
        return Ok(None);
    }

    // Parse all entries; first one is the persona base (or the generated entry if no
    // persona CODEX_CONFIG was set), rest are additional generated entries.
    let mut parsed_entries: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();
    for (i, raw) in codex_entries.iter().enumerate() {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serde_json::Value::Object(obj)) => parsed_entries.push(obj),
            Ok(_) => {
                let source = if i == 0 { "persona" } else { "generated" };
                return Err(AcpError::Protocol(format!(
                    "CODEX_CONFIG {source} value is valid JSON but not an object"
                )));
            }
            Err(e) => {
                let source = if i == 0 { "persona" } else { "generated" };
                return Err(AcpError::Protocol(format!(
                    "CODEX_CONFIG {source} value is not valid JSON: {e}"
                )));
            }
        }
    }

    // Start from first entry, deep-merge remaining entries.
    let mut base = parsed_entries.remove(0);
    for overlay in parsed_entries {
        deep_merge(&mut base, overlay);
    }

    // Deep-merge parent env (parent wins on colliding keys at every nesting level).
    if let Some(parent_raw) = parent_codex_config {
        match serde_json::from_str::<serde_json::Value>(parent_raw) {
            Ok(serde_json::Value::Object(parent_obj)) => {
                deep_merge(&mut base, parent_obj);
            }
            Ok(_) => {
                return Err(AcpError::Protocol(
                    "CODEX_CONFIG in parent environment is valid JSON but not an object".into(),
                ));
            }
            Err(e) => {
                return Err(AcpError::Protocol(format!(
                    "CODEX_CONFIG in parent environment is not valid JSON: {e}"
                )));
            }
        }
    }

    // Force sandbox_workspace_write.network_access = true (our invariant, always wins).
    let sws_entry = base
        .entry("sandbox_workspace_write")
        .or_insert_with(|| serde_json::json!({}));
    match sws_entry {
        serde_json::Value::Object(sws_obj) => {
            sws_obj.insert("network_access".to_string(), serde_json::Value::Bool(true));
        }
        other => {
            return Err(AcpError::Protocol(format!(
                "CODEX_CONFIG sandbox_workspace_write is not an object (got {}); \
                 cannot set network_access=true",
                other
            )));
        }
    }

    Ok(Some(serde_json::Value::Object(base).to_string()))
}

fn build_client_capabilities() -> serde_json::Value {
    serde_json::json!({
        // Signal to ACP adapters that Buzz can hand users to terminal-native
        // auth flows. Adapters decide which auth methods to expose; Buzz does
        // not hardcode vendor login commands from this capability.
        "auth": {
            "terminal": true
        },
        // Form elicitation: the agent may ask the owner a question mid-turn
        // and we render it as a channel message. `url` is deliberately absent
        // — Buzz has no browser-handoff surface, and claiming it would invite
        // url-mode requests we cannot complete.
        "elicitation": {
            "form": {}
        },
        // Signal to goose that we handle `_goose/unstable/session/update`
        // notifications. Without this the custom notification is suppressed
        // on goose's side and usage data is never emitted.
        "_meta": {
            "goose": {
                "customNotifications": true
            },
            // Non-standard extension used by claude-agent-acp to advertise the
            // exact terminal login argv for subscription auth. Unknown `_meta`
            // keys are ignored by other adapters.
            "terminal-auth": true
        }
    })
}

impl AcpClient {
    /// Kill the agent subprocess and wait for it to exit (no zombies).
    ///
    /// `Drop` only calls `start_kill()` (sends SIGKILL but doesn't reap).
    /// Call this when you need guaranteed cleanup — e.g., in `run_models`
    /// before process exit.
    pub async fn shutdown(&mut self) {
        // Kill the entire process group when possible. The child was spawned
        // with process_group(0), so its PID == its PGID. Killing the group
        // ensures subprocesses (MCP servers, tool processes) are cleaned up
        // rather than orphaned to init.
        //
        // Falls back to start_kill() (direct child only) on non-Unix or if
        // the child has been polled to completion (id() returns None).
        match self.child.id() {
            Some(pid) if kill_process_group(pid) => {}
            _ => {
                let _ = self.child.start_kill();
            }
        }
        // Bounded wait: if the child doesn't exit within 5s after SIGKILL,
        // give up and let Drop/OS handle it. An unbounded wait here would
        // wedge the harness during respawn or shutdown if a child is stuck.
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::debug!("child wait error after kill: {e}"),
            Err(_) => tracing::warn!("child did not exit within 5s after SIGKILL — abandoning"),
        }
    }

    /// Spawn the agent binary as a subprocess and connect to its stdio pipes.
    ///
    /// `has_generated_codex_config` must be true when `codex_network_env()` successfully
    /// injected a `CODEX_CONFIG` entry into `extra_env`.  The spawn path uses it to
    /// trigger the recursive merge + forced `network_access=true` in
    /// `build_codex_config_env`.  Pass `false` for test spawns and non-Codex agents.
    ///
    /// After spawning, call [`initialize`](Self::initialize) before any other method.
    pub async fn spawn(
        command: &str,
        args: &[String],
        extra_env: &[(String, String)],
        has_generated_codex_config: bool,
    ) -> Result<Self, AcpError> {
        use std::process::Stdio;

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherit stderr so agent logs are visible in the harness terminal.
            .stderr(Stdio::inherit())
            // Ensure the child is killed when the AcpClient is dropped (best-effort).
            // Callers MUST still call shutdown().await for guaranteed cleanup.
            .kill_on_drop(true);

        // Per-persona env vars (e.g., GOOSE_PROVIDER, BUZZ_AGENT_PROVIDER).
        // For most keys, operator precedence wins: skip injection if already set
        // in the parent environment.
        //
        // CODEX_CONFIG is handled specially via build_codex_config_env:
        //   • has_generated_codex_config=true: merge all CODEX_CONFIG entries + parent
        //     recursively and force network_access=true.
        //   • has_generated_codex_config=false: return None; any persona-supplied
        //     CODEX_CONFIG falls through to the normal operator-wins loop below.
        let has_codex_config = extra_env.iter().any(|(k, _)| k == "CODEX_CONFIG");
        let parent_codex_config = if has_generated_codex_config && has_codex_config {
            std::env::var("CODEX_CONFIG").ok()
        } else {
            None
        };
        let codex_config_value = build_codex_config_env(
            extra_env,
            parent_codex_config.as_deref(),
            has_generated_codex_config,
        )?;
        // When the merge path was not taken (None returned), any persona CODEX_CONFIG
        // entry falls through to the standard operator-wins treatment below.
        let codex_merge_active = codex_config_value.is_some();

        for (key, value) in extra_env {
            if key == "CODEX_CONFIG" && codex_merge_active {
                // Handled by build_codex_config_env; skip here to avoid double-setting.
                continue;
            }
            if std::env::var(key).is_err() {
                cmd.env(key, value);
            }
        }
        if let Some(merged) = codex_config_value {
            cmd.env("CODEX_CONFIG", merged);
        }

        // Spawn the agent in its own process group so SIGKILL doesn't propagate
        // to the harness's own process group on Unix.
        // tokio::process::Command::process_group is a stable tokio API (no extra imports needed).
        #[cfg(unix)]
        cmd.process_group(0);

        // Suppress the console window that Windows otherwise allocates for every
        // console-subsystem child process spawned from a GUI/non-console parent.
        configure_no_window(&mut cmd);

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Protocol("failed to open agent stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Protocol("failed to open agent stdout".into()))?;

        Ok(Self {
            child,
            stdin,
            reader: FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_SIZE)),
            next_id: 0,
            pending_permission_id: None,
            permission_responded: false,
            last_prompt_id: None,
            current_hard_deadline: None,
            observer: None,
            observer_agent_index: None,
            observer_context: ObserverContext::default(),
            active_run_id: None,
            steer_rx: None,
            elicitation: None,
            elicitation_rx: None,
            pending_elicitation: None,
            goose_usage: UsageTracker::default(),
        })
    }

    /// Attach a local observer feed to this ACP client.
    pub fn set_observer(&mut self, observer: Option<ObserverHandle>, agent_index: usize) {
        self.observer = observer;
        self.observer_agent_index = Some(agent_index);
    }

    /// Update metadata that will be attached to subsequent raw wire events.
    pub fn set_observer_context(&mut self, context: ObserverContext) {
        self.observer_context = context;
    }

    /// Return a clone of the observer handle, if attached.
    pub(crate) fn observer_handle(&self) -> Option<ObserverHandle> {
        self.observer.clone()
    }

    /// Return the pool slot index for this agent process.
    pub(crate) fn observer_agent_index(&self) -> Option<usize> {
        self.observer_agent_index
    }

    /// Emit a semantic event to the local observer feed, if enabled.
    pub fn observe(&self, kind: impl Into<String>, payload: serde_json::Value) {
        if let Some(observer) = &self.observer {
            observer.emit(
                kind,
                self.observer_agent_index,
                &self.observer_context,
                payload,
            );
        }
    }

    /// Send the `initialize` request and return the agent's response result value.
    ///
    /// Must be called exactly once, before any other ACP method.
    /// The caller may inspect `agentCapabilities` in the returned value.
    pub async fn initialize(&mut self) -> Result<serde_json::Value, AcpError> {
        // Requesting version 2 is an intentional temporary pin — we are squatting
        // on ACP v2 ahead of the upstream ACP RFD. Revisit when that RFD merges.
        let params = build_initialize_params();
        let result = self.send_request("initialize", params).await?;
        tracing::debug!(target: "buzz_acp::acp::init", "initialize response: {result}");
        Ok(result)
    }

    /// Send the ACP `authenticate` request for an adapter-advertised method.
    pub async fn authenticate(&mut self, method_id: &str) -> Result<serde_json::Value, AcpError> {
        let params = serde_json::json!({
            "methodId": method_id,
        });
        self.send_request("authenticate", params).await
    }

    /// Send `session/new` and return the full response alongside the session ID.
    ///
    /// `cwd` must be an absolute path. `mcp_servers` may be empty.
    /// `system_prompt` is included in the request when `Some` — agents that
    /// support the field will use it; others ignore unknown fields per JSON-RPC.
    /// Callers use [`extract_model_config_options`] and [`extract_model_state`]
    /// to pull model info from the raw result.
    pub async fn session_new_full(
        &mut self,
        cwd: &str,
        mcp_servers: Vec<McpServer>,
        system_prompt: Option<&str>,
    ) -> Result<SessionNewResponse, AcpError> {
        let mut params = serde_json::json!({
            "cwd": cwd,
            "mcpServers": mcp_servers,
        });
        if let Some(sp) = system_prompt {
            params["systemPrompt"] = serde_json::Value::String(sp.to_owned());
        }
        let result = self.send_request("session/new", params).await?;
        let session_id = result["sessionId"]
            .as_str()
            .ok_or_else(|| AcpError::Protocol("session/new response missing sessionId".into()))?
            .to_owned();
        tracing::info!(target: "buzz_acp::acp::session", "session created: {session_id}");
        Ok(SessionNewResponse {
            session_id,
            raw: result,
        })
    }

    /// Send `session/new` and return only the `sessionId` string.
    ///
    /// Convenience wrapper around [`session_new_full`].
    #[allow(dead_code)] // Public API — callers outside the harness may use this.
    pub async fn session_new(
        &mut self,
        cwd: &str,
        mcp_servers: Vec<McpServer>,
        system_prompt: Option<&str>,
    ) -> Result<String, AcpError> {
        Ok(self
            .session_new_full(cwd, mcp_servers, system_prompt)
            .await?
            .session_id)
    }

    /// Send `session/resume` to rebind an existing on-disk session.
    ///
    /// `cwd` must match the session's original working directory. Deliberately
    /// not `session/load`: that replays the whole prior conversation as
    /// `session/update` notifications, which the read loop would mirror into the
    /// observer feed. Resume rebinds without replay.
    ///
    /// The response carries the same `modes`/`configOptions` shape as
    /// `session/new` but need not repeat `sessionId`, so the requested ID is the
    /// fallback.
    pub async fn session_resume(
        &mut self,
        cwd: &str,
        session_id: &str,
        mcp_servers: Vec<McpServer>,
    ) -> Result<SessionNewResponse, AcpError> {
        let params = serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd,
            "mcpServers": mcp_servers,
        });
        let result = self.send_request("session/resume", params).await?;
        let resumed = result["sessionId"]
            .as_str()
            .unwrap_or(session_id)
            .to_owned();
        tracing::info!(target: "buzz_acp::acp::session", "session resumed: {resumed}");
        Ok(SessionNewResponse {
            session_id: resumed,
            raw: result,
        })
    }

    /// Send `session/close` to release the agent-side resources of a session
    /// this harness is done with.
    ///
    /// Per the ACP spec the agent cancels any in-flight work and frees the
    /// session; for subprocess-backed adapters that is what terminates the
    /// per-session harness child. Dropping a session ID without closing it
    /// strands that child for the adapter's whole lifetime.
    ///
    /// Deliberately not `session/delete`: close releases the live session while
    /// leaving the on-disk transcript intact, so a binding retained across the
    /// close (a model switch, say) can still be resumed.
    pub async fn session_close(&mut self, session_id: &str) -> Result<(), AcpError> {
        let params = serde_json::json!({ "sessionId": session_id });
        self.send_request("session/close", params).await?;
        tracing::debug!(target: "buzz_acp::acp::session", "session closed: {session_id}");
        Ok(())
    }

    /// Send Goose's custom system-prompt request after `session/new`.
    pub async fn session_set_goose_system_prompt(
        &mut self,
        session_id: &str,
        text: &str,
    ) -> Result<serde_json::Value, AcpError> {
        self.send_request(
            "_goose/unstable/session/system-prompt/set",
            serde_json::json!({
                "sessionId": session_id,
                "mode": "append",
                "key": "buzz",
                "text": text,
            }),
        )
        .await
    }

    /// Send `session/set_config_option` (stable ACP path).
    pub async fn session_set_config_option(
        &mut self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<serde_json::Value, AcpError> {
        let params = serde_json::json!({
            "sessionId": session_id,
            "configId": config_id,
            "value": value,
        });
        self.send_request("session/set_config_option", params).await
    }

    /// Send `session/set_model` (unstable ACP path).
    pub async fn session_set_model(
        &mut self,
        session_id: &str,
        model_id: &str,
    ) -> Result<serde_json::Value, AcpError> {
        let params = serde_json::json!({
            "sessionId": session_id,
            "modelId": model_id,
        });
        self.send_request("session/set_model", params).await
    }

    /// Send `session/prompt` with idle-based timeout instead of wall-clock.
    ///
    /// The idle deadline resets on any stdout activity from the agent. The hard
    /// deadline is an absolute wall-clock cap (safety valve).
    pub async fn session_prompt_with_idle_timeout(
        &mut self,
        session_id: &str,
        prompt_text: &str,
        idle_timeout: std::time::Duration,
        max_duration: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        self.session_prompt_blocks_with_idle_timeout(
            session_id,
            std::slice::from_ref(&prompt_text),
            idle_timeout,
            max_duration,
        )
        .await
    }

    /// Like [`session_prompt_with_idle_timeout`](Self::session_prompt_with_idle_timeout),
    /// but sends each entry in `prompt_blocks` as a separate text content block.
    ///
    /// Used for slash-command pass-through: ACP connectors detect commands via
    /// the **first** block's text starting with `/`, so the harness sends
    /// `["/cmd args", "<buzz context>"]` instead of one wrapped block.
    pub async fn session_prompt_blocks_with_idle_timeout(
        &mut self,
        session_id: &str,
        prompt_blocks: &[&str],
        idle_timeout: std::time::Duration,
        max_duration: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        let params = build_prompt_params(session_id, prompt_blocks);
        let hard_deadline = tokio::time::Instant::now() + max_duration;
        self.current_hard_deadline = Some(hard_deadline);

        // Mark the usage tracker as in-flight for this turn BEFORE sending the
        // prompt so that any setup notifications recorded earlier are not
        // misattributed to this turn.
        self.goose_usage.begin_turn(session_id);

        self.last_prompt_id = Some(self.next_id);
        let id = self.next_id;
        self.next_id += 1;

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": params,
        });

        tracing::debug!(target: "buzz_acp::acp::wire", "→ {}", &serde_json::to_string(&msg).unwrap_or_default());
        if let Err(e) = self.write_ndjson(&msg).await {
            self.last_prompt_id = None;
            self.current_hard_deadline = None;
            return Err(e);
        }

        let result = self
            .read_until_response_with_idle_timeout(
                session_id,
                id,
                idle_timeout,
                hard_deadline,
                max_duration,
            )
            .await;

        // On timeout errors, leave current_hard_deadline set so cancel_with_cleanup
        // can inherit the remaining budget. Clear it on all other outcomes.
        match &result {
            Ok(_) => {
                self.last_prompt_id = None;
                self.current_hard_deadline = None;
            }
            Err(AcpError::IdleTimeout(_) | AcpError::HardTimeout { .. }) => {
                // Leave last_prompt_id and current_hard_deadline set —
                // caller will invoke cancel_with_cleanup.
            }
            Err(_) => {
                self.last_prompt_id = None;
                self.current_hard_deadline = None;
            }
        }
        self.parse_stop_reason(&result?)
    }

    /// Send a `session/cancel` **notification** (no `id` field, no response expected).
    ///
    /// After calling this, the agent will eventually respond to the in-flight
    /// `session/prompt` with `stopReason: "cancelled"`. Use
    /// [`cancel_with_cleanup`](Self::cancel_with_cleanup) if you need to drain
    /// that response.
    ///
    /// Note: async because writing to stdin requires async I/O.
    pub async fn session_cancel(&mut self, session_id: &str) -> Result<(), AcpError> {
        let params = serde_json::json!({
            "sessionId": session_id,
        });
        self.send_notification("session/cancel", params).await
    }

    /// Returns `true` if a `session/prompt` request is currently in flight.
    pub fn has_in_flight_prompt(&self) -> bool {
        self.last_prompt_id.is_some()
    }

    /// Most recently observed goose `_meta.goose.activeRunId` from a
    /// `session_info_update`, if any.
    ///
    /// Both goose and buzz-agent emit `session_info_update`; other agents
    /// leave this `None` for the lifetime of the client. Read directly by
    /// `read_until_response_with_idle_timeout`'s
    /// steer arm at write time (see [`crate::pool::SteerRequest`] for
    /// why the read loop owns this); production callers do not need this
    /// accessor. Kept as `pub` so tests can introspect the field.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn active_run_id(&self) -> Option<&str> {
        self.active_run_id.as_deref()
    }

    /// Consume and return the per-turn usage record computed from the most
    /// recent `_goose/unstable/session/update` notification.
    ///
    /// Returns `None` if no usage update arrived since the last call (i.e.
    /// the harness did not emit one for this turn, or this is not a goose
    /// agent). Must be called at most once per turn; subsequent calls return
    /// `None` until the next `usage_update` notification is recorded.
    ///
    /// Intended for consumption by `publish_agent_turn_metric` in `pool.rs` to
    /// publish a kind 44200 NIP-AM event.
    pub fn take_turn_usage(&mut self) -> Option<TurnUsage> {
        self.goose_usage.take()
    }

    /// Install a per-turn steer request channel for goose-native
    /// non-cancelling mid-turn delivery.
    ///
    /// Called by the dispatch path immediately before
    /// [`session_prompt_with_idle_timeout`] for all prompt tasks.
    /// The matching `Sender` is stored in `TaskMeta.steer_tx` for the
    /// main loop's mode-gate fork to drive.
    ///
    /// Panics if a receiver is already installed — there is exactly one
    /// turn per `AcpClient` at a time, and stacking receivers would
    /// silently misroute steer requests across turns. The previous
    /// turn's receiver must have been consumed by the read loop and
    /// dropped at scope exit before the next turn dispatches.
    pub fn install_steer_rx(&mut self, rx: tokio::sync::mpsc::Receiver<crate::pool::SteerRequest>) {
        assert!(
            self.steer_rx.is_none(),
            "install_steer_rx: previous turn's receiver was not consumed — \
             stacking receivers would misroute steer requests across turns"
        );
        self.steer_rx = Some(rx);
    }

    /// Install the per-turn elicitation plumbing: the surface an agent question
    /// is published to, and the channel the owner's reply arrives on.
    ///
    /// Panics for the same reason [`install_steer_rx`](Self::install_steer_rx)
    /// does — one turn per `AcpClient` at a time, and a stacked channel would
    /// answer this turn's question into the previous turn's thread.
    pub fn install_elicitation(
        &mut self,
        ask: crate::pool::ElicitationAsk,
        rx: tokio::sync::mpsc::Receiver<crate::pool::ElicitationReply>,
    ) {
        assert!(
            self.elicitation.is_none(),
            "install_elicitation: previous turn's ask surface was not cleared — \
             stacking them would misroute owner replies across turns"
        );
        self.elicitation = Some(ask);
        self.elicitation_rx = Some(rx);
    }

    /// Drop the per-turn elicitation plumbing and any question parked on it.
    ///
    /// Called by `send_prompt_result` alongside [`clear_steer_rx`](Self::clear_steer_rx)
    /// so `install_elicitation`'s invariant holds for the next dispatch, and so
    /// a question that outlived its turn can never be answered into the next
    /// one. Idempotent.
    pub fn clear_elicitation(&mut self) {
        self.take_pending_elicitation();
        self.elicitation = None;
        self.elicitation_rx = None;
    }

    /// Take the parked question, disarming the shared state the main loop
    /// checks before it routes an owner message as a reply.
    fn take_pending_elicitation(&mut self) -> Option<PendingElicitation> {
        if let Some(ask) = self.elicitation.as_ref() {
            ask.disarm();
        }
        self.pending_elicitation.take()
    }

    /// Publish the parked question's current field and arm the shared state so
    /// the main loop routes the owner's reply here.
    ///
    /// A question that never reached the relay is one nobody can answer, so on
    /// publish failure the parked request is answered `cancel` immediately —
    /// the agent reports an aborted tool call within seconds instead of the
    /// turn parking until its hard cap.
    async fn ask_parked_elicitation(&mut self) -> Result<(), AcpError> {
        let (Some(ask), Some(pending)) = (&self.elicitation, &self.pending_elicitation) else {
            return Ok(());
        };
        let field = &pending.fields[pending.asking];
        let total = pending.fields.len();
        let body = render_elicitation_field(field, pending.asking, total);
        let ask_tag = elicitation_ask_tag(field, pending.asking, total);
        if ask.publish(body, ask_tag).await {
            return Ok(());
        }
        let Some(pending) = self.take_pending_elicitation() else {
            return Ok(());
        };
        tracing::warn!(
            target: "buzz_acp::acp::elicitation",
            "publishing the question for elicitation id={} failed — cancelling it",
            pending.id
        );
        self.write_ndjson(&elicitation_response(&pending.id, "cancel", None))
            .await
    }

    /// Clear any installed steer receiver without consuming it.
    ///
    /// Called by `send_prompt_result` on every exit path of `run_prompt_task`
    /// so that `install_steer_rx`'s `is_none()` invariant holds for the next
    /// dispatch even when the turn ended before the read loop ran `take()`.
    /// Idempotent — safe to call when `steer_rx` is already `None`.
    pub fn clear_steer_rx(&mut self) {
        self.steer_rx = None;
    }

    /// Returns `true` if no steer receiver is currently installed.
    ///
    /// Test-only: used by `pool` tests to assert the post-return invariant
    /// without exposing the private field directly.
    #[cfg(test)]
    pub fn steer_rx_is_none(&self) -> bool {
        self.steer_rx.is_none()
    }

    /// Cancel a turn cleanly, handling any pending permission request first.
    ///
    /// Steps:
    /// 1. If there is a pending `session/request_permission` that hasn't been
    ///    responded to yet, respond with `outcome: "cancelled"`.
    /// 2. Send `session/cancel` notification (no id).
    /// 3. Continue reading until the `session/prompt` response arrives with `stopReason: "cancelled"`.
    ///
    /// Returns the final [`StopReason`] (almost always [`StopReason::Cancelled`]).
    pub async fn cancel_with_cleanup(
        &mut self,
        session_id: &str,
        _idle_timeout: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        // Inherit the hard deadline from the timed-out turn so the drain loop
        // doesn't start a fresh timer (prevents double-jeopardy). If the original
        // deadline is already expired or near-expired, grant a 30s floor so the
        // cancel notification has time to propagate and the agent can respond.
        let stored_deadline = self.current_hard_deadline.take();
        let min_cleanup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let hard_deadline = match stored_deadline {
            Some(d) if d > min_cleanup_deadline => d,
            Some(_) => {
                tracing::debug!(
                    "original hard deadline expired or near-expired — using 30s cleanup grace"
                );
                min_cleanup_deadline
            }
            None => {
                tracing::warn!(
                    "cancel_with_cleanup called without current_hard_deadline — using 30s fallback"
                );
                min_cleanup_deadline
            }
        };

        self.cancel_with_cleanup_until(session_id, hard_deadline)
            .await
    }

    /// Cancel a user-interrupted turn with a bounded grace window.
    ///
    /// Some ACP servers currently keep streaming after `session/cancel`. For an
    /// explicit Stop button, waiting until the original turn deadline can make
    /// cancellation look broken. This variant gives the agent a short chance to
    /// acknowledge cancellation, then returns a timeout so the caller can respawn
    /// the agent process and actually stop the work.
    ///
    /// The `grace` window is a cleanup deadline, not the turn's real max-turn
    /// wall clock — a bounded drain that expires maps to
    /// [`AcpError::CancelDrainTimeout`], never [`AcpError::HardTimeout`], so
    /// callers can distinguish "agent didn't stop in time" from a genuine
    /// configured hard-cap breach.
    pub async fn cancel_with_cleanup_grace(
        &mut self,
        session_id: &str,
        grace: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        let _ = self.current_hard_deadline.take();
        let hard_deadline = tokio::time::Instant::now() + grace;
        match self
            .cancel_with_cleanup_until(session_id, hard_deadline)
            .await
        {
            Err(AcpError::HardTimeout { .. }) => Err(AcpError::CancelDrainTimeout(grace)),
            other => other,
        }
    }

    async fn cancel_with_cleanup_until(
        &mut self,
        session_id: &str,
        hard_deadline: tokio::time::Instant,
    ) -> Result<StopReason, AcpError> {
        // Validate precondition before any side effects — fail fast if there's
        // no in-flight prompt (prevents writing permission responses or cancel
        // notifications to the agent when no prompt is active).
        let prompt_id = self.last_prompt_id.take().ok_or_else(|| {
            AcpError::Protocol("cancel_with_cleanup called with no in-flight prompt".into())
        })?;

        // Step 1: respond to any pending permission request with "cancelled",
        // but only if we haven't already responded (guards against double-response race).
        if let Some(perm_id) = self.pending_permission_id.clone() {
            if !self.permission_responded {
                let response = permission_response_cancelled(&perm_id);
                self.write_ndjson(&response).await?;
                tracing::debug!(
                    target: "buzz_acp::acp::cancel",
                    "responded cancelled to pending permission id={perm_id}"
                );
            }
            self.pending_permission_id = None;
            self.permission_responded = false;
        }

        // Step 1b: same for a question the owner never answered. Without this
        // the agent stays blocked on its `elicitation/create` and never
        // acknowledges the cancel.
        if let Some(pending) = self.take_pending_elicitation() {
            let response = elicitation_response(&pending.id, "cancel", None);
            self.write_ndjson(&response).await?;
            tracing::debug!(
                target: "buzz_acp::acp::cancel",
                "cancelled pending elicitation id={}", pending.id
            );
        }

        // Step 2: send session/cancel notification (no id)
        self.session_cancel(session_id).await?;
        tracing::info!(target: "buzz_acp::acp::cancel", "sent session/cancel for {session_id}");
        // Use a fixed 30s idle timeout during cleanup — the cancel notification
        // needs time to propagate and the agent may go silent while winding down.
        // The separate hard_deadline bounds agents that keep producing output
        // but ignore cancellation.
        let cleanup_idle = std::time::Duration::from_secs(30);
        let remaining = hard_deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_default();
        let result = self
            .read_until_response_with_idle_timeout(
                session_id,
                prompt_id,
                cleanup_idle,
                hard_deadline,
                remaining,
            )
            .await?;
        self.parse_stop_reason(&result)
    }

    /// Serialize `value` as a single NDJSON line and flush to the agent's stdin.
    ///
    /// Bounded by a 30-second write timeout. If the agent stops reading stdin
    /// (e.g., it's stuck or dead), the write would otherwise block forever.
    async fn write_ndjson(&mut self, value: &serde_json::Value) -> Result<(), AcpError> {
        const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let line = serde_json::to_string(value)?;
        tokio::time::timeout(WRITE_TIMEOUT, async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.write_all(b"\n").await?;
            self.stdin.flush().await?;
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|_| AcpError::WriteTimeout(WRITE_TIMEOUT))?
        .map_err(AcpError::Io)?;
        self.observe("acp_write", value.clone());
        Ok(())
    }

    /// Default timeout for non-prompt RPCs (initialize, session/new, etc.).
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Send a JSON-RPC request and wait for the matching response.
    ///
    /// Assigns the next available id, writes the NDJSON line to stdin,
    /// then calls [`read_until_response`](Self::read_until_response).
    ///
    /// The write phase is bounded by `WRITE_TIMEOUT` (30s) and the read phase
    /// by `REQUEST_TIMEOUT` (60s), so worst-case wall clock is ~90s. Non-prompt
    /// RPCs like `initialize` and `session/new` should complete in seconds;
    /// if they don't, the agent is likely stuck and we must not block forever.
    async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AcpError> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        tracing::debug!(target: "buzz_acp::acp::wire", "→ {}", &serde_json::to_string(&msg).unwrap_or_default());

        // Wrap write + read in a single timeout so a hung agent can't block forever.
        // We cannot use an async block that borrows `self` mutably across two awaits
        // inside timeout(), so we sequence them with early-return on timeout.
        let timeout = Self::REQUEST_TIMEOUT;
        match tokio::time::timeout(timeout, self.write_ndjson(&msg)).await {
            Ok(result) => result?,
            Err(_) => return Err(AcpError::Timeout(timeout)),
        }

        match tokio::time::timeout(timeout, self.read_until_response(id)).await {
            Ok(result) => result,
            Err(_) => Err(AcpError::Timeout(timeout)),
        }
    }

    /// Drain any buffered lines from the agent's stdout without blocking.
    ///
    /// After a [`AcpError::Timeout`] from [`send_request`], the agent may
    /// eventually send the late response. That stale message will sit in the
    /// `BufReader` buffer and be silently skipped by the next `read_until_response`
    /// call (ID mismatch). However, if the caller wants a clean slate — e.g.
    /// before retrying the same method — they can call this to consume any
    /// buffered data with a short deadline.
    ///
    /// This is a best-effort drain: it reads until the buffer is empty or
    /// `drain_timeout` elapses, whichever comes first. Errors are ignored.
    #[allow(dead_code)] // Scaffolding for future model-switch timeout cleanup; not yet wired.
    pub async fn drain_stale_responses(&mut self, drain_timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + drain_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let read_result = tokio::time::timeout(remaining, self.reader.next()).await;
            match read_result {
                // Timeout or stream ended — buffer is empty or agent exited.
                Err(_) | Ok(None) => break,
                Ok(Some(Ok(_))) => {
                    // Consumed one buffered line; loop to drain more.
                    tracing::debug!(target: "buzz_acp::acp::wire", "drained stale buffered line");
                }
                Ok(Some(Err(_))) => break,
            }
        }
    }

    /// Send a JSON-RPC **notification** — no `id` field, no response expected.
    ///
    /// Used for `session/cancel`. The absence of `id` is the JSON-RPC 2.0
    /// distinguisher between requests and notifications.
    async fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), AcpError> {
        // Notifications deliberately have NO "id" field.
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        tracing::debug!(target: "buzz_acp::acp::wire", "→ (notification) {}", &serde_json::to_string(&msg).unwrap_or_default());
        self.write_ndjson(&msg).await?;
        Ok(())
    }

    /// Core message loop: read NDJSON lines until we get a response matching `expected_id`.
    ///
    /// While waiting, handles:
    /// - `session/update` notifications → logged via tracing
    /// - `session/request_permission` requests → auto-approved with `allow_once`
    /// - Any other messages → debug-logged and ignored; if they carry an `id`
    ///   (i.e. they are requests, not notifications), a JSON-RPC -32601 error is sent.
    ///
    /// Compares the incoming `id` field as a `serde_json::Value` against
    /// `json!(expected_id)` so that both numeric and string IDs work correctly.
    async fn read_until_response(
        &mut self,
        expected_id: u64,
    ) -> Result<serde_json::Value, AcpError> {
        loop {
            // LinesCodec::new_with_max_length enforces MAX_LINE_SIZE at the
            // read level — the buffer never grows beyond the limit, preventing
            // OOM from rogue agents writing infinite non-newline bytes.
            let line = match self.reader.next().await {
                None => return Err(AcpError::AgentExited),
                Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                    return Err(AcpError::Protocol(
                        "agent stdout line exceeded 10MB limit".into(),
                    ));
                }
                Some(Err(e)) => {
                    return Err(AcpError::Io(std::io::Error::other(e)));
                }
                Some(Ok(line)) => line,
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Only log and reset idle after we have a valid non-empty line.
            tracing::debug!(target: "buzz_acp::acp::wire", "← {trimmed}");

            let msg: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    self.observe(
                        "acp_parse_error",
                        serde_json::json!({
                            "line": trimmed,
                            "error": e.to_string(),
                        }),
                    );
                    tracing::warn!(
                        target: "buzz_acp::acp::wire",
                        "failed to parse line as JSON: {e} — skipping"
                    );
                    continue;
                }
            };
            self.observe("acp_read", msg.clone());

            // Check if this is a response to our expected request (has matching id
            // AND no `method` field — a `method` field means it's an agent-initiated
            // request, not a response, even if the id happens to match).
            if let Some(id) = msg.get("id") {
                if *id == serde_json::json!(expected_id) && msg.get("method").is_none() {
                    if let Some(error) = msg.get("error") {
                        return Err(agent_error_from_json(error));
                    }
                    return Ok(msg["result"].clone());
                }
            }

            // Dispatch by method name (notifications and agent-initiated requests).
            if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                match method {
                    "session/update" => {
                        let _ = self.handle_session_update(&msg);
                    }
                    "_goose/unstable/session/update" => {
                        self.handle_goose_usage_update(&msg);
                    }
                    "session/request_permission" => {
                        self.handle_permission_request(&msg).await?;
                    }
                    // Nothing polls a parked question outside the prompt loop —
                    // `initialize` and `session/new` run here — so the request
                    // is never answerable and is cancelled immediately.
                    "elicitation/create" => {
                        self.handle_elicitation_request(&msg, false).await?;
                    }
                    other => {
                        // If the unknown message has an id, it's a request expecting a reply.
                        // Silence would cause the agent to hang waiting for a response.
                        // Send a JSON-RPC -32601 "Method not found" error.
                        if msg.get("id").is_some() {
                            let err_resp = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "error": {"code": -32601, "message": format!("Method not found: {other}")}
                            });
                            // Surface write failures — a broken pipe means the
                            // agent process is dead and continuing would hang.
                            self.write_ndjson(&err_resp).await?;
                        }
                        tracing::debug!(target: "buzz_acp::acp::wire", "ignoring unknown method: {other}");
                    }
                }
            }
        }
    }

    /// Idle-aware message loop: like [`read_until_response`] but resets an idle
    /// deadline on every stdout line. Fires [`AcpError::IdleTimeout`] on silence
    /// or [`AcpError::HardTimeout`] on absolute wall-clock cap.
    ///
    /// `hard_deadline` is an absolute `Instant` (pre-computed by the caller) so
    /// that `cancel_with_cleanup` can inherit the remaining budget from the
    /// original turn rather than starting a fresh timer.
    /// Read agent messages until the response with `expected_id` arrives, or
    /// either of two timeouts fires. Returns `Result<value, IdleTimeout |
    /// HardTimeout | other>`.
    ///
    /// - `idle_timeout`: silent-agent guard, **reset on every line of valid
    ///   JSON** (and explicitly on `session/update` notifications).
    /// - `hard_deadline`: absolute wall-clock cap on the whole call, passed
    ///   in so that `cancel_with_cleanup` can inherit the remaining budget
    ///   from the original turn rather than starting a fresh timer.
    ///
    /// While reading, the loop interleaves goose-native non-cancelling steer
    /// requests via `tokio::select!`. The select uses `biased` for
    /// reader-first throughput, with a pre-select deadline check at the top
    /// of every loop iteration so a continuously-ready reader arm cannot
    /// starve the hard deadline (Max's review gate). The steer arm is
    /// guarded by `pending_steer.is_none()` so at most one steer is in
    /// flight at a time; a successful steer response is routed to the
    /// caller's oneshot ack instead of being returned as the prompt result.
    ///
    /// `session_id` is threaded in lexically by callers so the goose-native
    /// steer arm can complete `sessionId` in the steer JSON-RPC params at
    /// write time without needing access to outer state. See
    /// [`crate::pool::SteerRequest`] for why params are built here and not
    /// in the main loop.
    async fn read_until_response_with_idle_timeout(
        &mut self,
        session_id: &str,
        expected_id: u64,
        idle_timeout: std::time::Duration,
        hard_deadline: tokio::time::Instant,
        max_duration: std::time::Duration,
    ) -> Result<serde_json::Value, AcpError> {
        use tokio::time::Instant;

        // Take the per-turn steer receiver into a local so it can be
        // borrowed independently of `self.reader` inside `select!`.
        // Dropped at scope exit (return paths drain `pending_steer` first
        // so the ack_tx oneshot is never leaked silently).
        let mut steer_rx = self.steer_rx.take();

        // Same treatment for the per-turn owner-reply channel.
        let mut elicitation_rx = self.elicitation_rx.take();

        // Tracks the in-flight steer write: `(request_id, ack_tx)`. While
        // `Some`, the steer arm is gated off so we don't stack writes,
        // and a response matching `id` is routed to the ack_tx instead
        // of being treated as the prompt result. Drained on every return
        // path with `PromptCompletedNeutral` so callers are never left
        // hanging.
        let mut pending_steer: Option<(u64, tokio::sync::oneshot::Sender<crate::pool::SteerAck>)> =
            None;

        let now = Instant::now();
        let mut idle_deadline = now + idle_timeout;
        let mut hard_deadline = hard_deadline;
        let mut last_activity_at = now;

        loop {
            // A parked elicitation waits on a human, not the agent, so the
            // silent-agent guard is suspended for as long as one is
            // outstanding — otherwise anyone who thinks for longer than
            // `idle_timeout` kills the turn. The hard deadline still applies,
            // so an unanswered question cannot pin an agent slot forever, and
            // agent death is still caught immediately by reader EOF.
            let elicitation_parked = self.pending_elicitation.is_some();

            // With no question parked, nothing in the reply channel can be an
            // answer: it is a reply the main loop sent for a question that was
            // cancelled (by the agent, or by a failed publish) before the read
            // loop consumed it. Discard it the moment the question stops being
            // answerable, so it can neither be misattributed to the next
            // question nor sit in the capacity-1 channel blocking the main
            // loop's next `try_send`.
            if !elicitation_parked {
                if let Some(rx) = elicitation_rx.as_mut() {
                    while rx.try_recv().is_ok() {}
                }
            }

            // Determine which deadline fires first BEFORE sleeping — this is
            // the classification we'll use on timeout, immune to scheduler jitter.
            let idle_fires_first = idle_deadline < hard_deadline && !elicitation_parked;
            let next_deadline = if idle_fires_first {
                idle_deadline
            } else {
                hard_deadline
            };

            // Pre-select deadline check — required by Max's review. Under
            // `biased`, a continuously-ready reader arm wins every poll and
            // `sleep_until(next_deadline)` is never reached, silently
            // defeating the hard-deadline guarantee for agents that keep
            // producing output (see `acp.rs:608` for why the hard deadline
            // exists). Check the classified deadline here so a steady-
            // stream agent is still bounded.
            if Instant::now() >= next_deadline {
                if let Some((_, ack_tx)) = pending_steer.take() {
                    // Prompt is timing out — release the withheld event via
                    // PromptCompletedNeutral (no fallback signal: there is
                    // no in-flight turn to signal once we return, and
                    // normal dispatch handles redelivery).
                    let _ = ack_tx.send(crate::pool::SteerAck::PromptCompletedNeutral);
                }
                if idle_fires_first {
                    tracing::warn!("idle timeout ({idle_timeout:?}) — no agent activity");
                    return Err(AcpError::IdleTimeout(idle_timeout));
                } else {
                    let silence = Instant::now().saturating_duration_since(last_activity_at);
                    tracing::warn!("hard turn timeout exceeded (silence {silence:?})");
                    return Err(AcpError::HardTimeout { silence });
                }
            }

            // LinesCodec::new_with_max_length enforces MAX_LINE_SIZE at the
            // read level — the buffer never grows beyond the limit.
            let read_result = tokio::select! {
                biased;
                read_result = self.reader.next() => Some(read_result),
                // Steer arm: gated off whenever a steer write is already in
                // flight so we don't stack two writes against the same
                // process. The `async { steer_rx.as_mut()?.recv().await }`
                // wrapper produces `None` when no receiver is installed,
                // which mismatches the `Some(req)` pattern and disables the
                // branch for that iteration (no busy loop). Cancel-safe:
                // `mpsc::Receiver::recv` does not lose messages on drop.
                Some(req) = async {
                    match steer_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                }, if pending_steer.is_none() => {
                    // Selected: build steer params at write time using the
                    // lexical `session_id` and the freshest `active_run_id`.
                    //
                    // `active_run_id` is updated by `session/update`
                    // notifications inside this very loop; reading it here
                    // (rather than snapshotting at dispatch) guarantees the
                    // value matches what goose's run-id check will compare
                    // against. If it's `None`, no `session/update` has
                    // arrived yet so we cannot form a valid `expectedRunId`
                    // — ack `ExpectedRunIdMissing` and drop the request
                    // without writing anything. The main loop maps this to
                    // the universal cancel+merge `Steer` fallback.
                    match self.active_run_id.clone() {
                        None => {
                            tracing::warn!(
                                "goose-native steer: no active_run_id at write time \
                                 (no session/update seen yet) — falling back to cancel+merge"
                            );
                            let _ = req.ack_tx.send(crate::pool::SteerAck::Err(
                                crate::pool::SteerError::ExpectedRunIdMissing,
                            ));
                        }
                        Some(run_id) => {
                            let id = self.next_id;
                            self.next_id += 1;
                            let prompt_block_refs: Vec<&str> =
                                req.prompt_blocks.iter().map(String::as_str).collect();
                            let params =
                                build_steer_params(session_id, &run_id, &prompt_block_refs);
                            let msg = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "method": "_goose/unstable/session/steer",
                                "params": params,
                            });
                            tracing::debug!(
                                target: "buzz_acp::acp::wire",
                                "→ {}",
                                serde_json::to_string(&msg).unwrap_or_default()
                            );
                            match self.write_ndjson(&msg).await {
                                Ok(()) => {
                                    pending_steer = Some((id, req.ack_tx));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "goose-native steer write failed: {e} — releasing withheld event"
                                    );
                                    let _ = req.ack_tx.send(crate::pool::SteerAck::Err(
                                        crate::pool::SteerError::Transport(e.to_string()),
                                    ));
                                }
                            }
                        }
                    }
                    // Loop back to the next iteration without consuming a
                    // reader line; we'll wait for either the prompt
                    // response or the steer response next.
                    None
                }
                // Elicitation arm: gated on a parked question so an owner
                // message that races the agent's request is never consumed as
                // an answer to it. Same `async {}` no-receiver wrapper and
                // cancel-safety as the steer arm above.
                Some(reply) = async {
                    match elicitation_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                }, if elicitation_parked => {
                    self.apply_elicitation_reply(reply).await?;
                    // The human is no longer holding the turn open, so restart
                    // the silent-agent guard from now rather than from the
                    // stale pre-question deadline.
                    idle_deadline = Instant::now() + idle_timeout;
                    None
                }
                _ = tokio::time::sleep_until(next_deadline) => {
                    // The pre-select check at the top of the next iteration
                    // would catch this anyway, but firing the deadline arm
                    // here makes the wakeup immediate (no extra reader poll
                    // round-trip when stdout is idle).
                    if let Some((_, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::pool::SteerAck::PromptCompletedNeutral);
                    }
                    if idle_fires_first {
                        tracing::warn!("idle timeout ({idle_timeout:?}) — no agent activity");
                        return Err(AcpError::IdleTimeout(idle_timeout));
                    } else {
                        let silence = Instant::now().saturating_duration_since(last_activity_at);
                        tracing::warn!("hard turn timeout exceeded (silence {silence:?})");
                        return Err(AcpError::HardTimeout { silence });
                    }
                }
            };

            // Steer arm fired (or the select selected nothing read-side this
            // iteration): no reader frame to process, loop to re-evaluate
            // deadlines and arm the next select.
            let read_result = match read_result {
                Some(r) => r,
                None => continue,
            };

            match read_result {
                None => {
                    if let Some((_, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::pool::SteerAck::PromptCompletedNeutral);
                    }
                    return Err(AcpError::AgentExited);
                }
                Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                    if let Some((_, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::pool::SteerAck::PromptCompletedNeutral);
                    }
                    return Err(AcpError::Protocol(
                        "agent stdout line exceeded 10MB limit".into(),
                    ));
                }
                Some(Err(e)) => {
                    if let Some((_, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::pool::SteerAck::PromptCompletedNeutral);
                    }
                    return Err(AcpError::Io(std::io::Error::other(e)));
                }
                Some(Ok(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    tracing::debug!(target: "buzz_acp::acp::wire", "← {trimmed}");

                    let msg: serde_json::Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(e) => {
                            self.observe(
                                "acp_parse_error",
                                serde_json::json!({
                                    "line": trimmed,
                                    "error": e.to_string(),
                                }),
                            );
                            tracing::warn!(
                                target: "buzz_acp::acp::wire",
                                "failed to parse line as JSON: {e} — skipping"
                            );
                            continue;
                        }
                    };
                    self.observe("acp_read", msg.clone());

                    let activity_now = Instant::now();
                    idle_deadline = activity_now + idle_timeout;
                    last_activity_at = activity_now;

                    // Steer response routing must come BEFORE the prompt
                    // response check: a steer response is a regular
                    // JSON-RPC response (id + result/error, no method),
                    // so the matcher must disambiguate by id. Both checks
                    // share the `no method` guard.
                    if let Some(id) = msg.get("id") {
                        if msg.get("method").is_none() {
                            if let Some((steer_id, _)) = pending_steer.as_ref() {
                                if *id == serde_json::json!(*steer_id) {
                                    // Take the ack_tx out and route the
                                    // response. We do not return — keep
                                    // reading until the prompt response
                                    // arrives.
                                    let (_, ack_tx) = pending_steer.take().expect("just checked");
                                    let ack = if let Some(error) = msg.get("error") {
                                        let code = error
                                            .get("code")
                                            .and_then(|c| c.as_i64())
                                            .unwrap_or(-1);
                                        let message = error.to_string();
                                        crate::pool::SteerAck::Err(
                                            crate::pool::SteerError::AgentError { code, message },
                                        )
                                    } else {
                                        let renew_now = Instant::now();
                                        let new_deadline = renew_now + max_duration;
                                        if new_deadline > hard_deadline {
                                            hard_deadline = new_deadline;
                                            self.current_hard_deadline = Some(new_deadline);
                                            tracing::info!(
                                                "steer success: renewed hard deadline ({max_duration:?} from now)"
                                            );
                                        }
                                        crate::pool::SteerAck::Success
                                    };
                                    let _ = ack_tx.send(ack);
                                    continue;
                                }
                            }
                            if *id == serde_json::json!(expected_id) {
                                if let Some(error) = msg.get("error") {
                                    if let Some((_, ack_tx)) = pending_steer.take() {
                                        let _ = ack_tx
                                            .send(crate::pool::SteerAck::PromptCompletedNeutral);
                                    }
                                    return Err(agent_error_from_json(error));
                                }
                                if let Some((_, ack_tx)) = pending_steer.take() {
                                    let _ =
                                        ack_tx.send(crate::pool::SteerAck::PromptCompletedNeutral);
                                }
                                return Ok(msg["result"].clone());
                            }
                        }
                    }

                    // Dispatch notifications and agent-initiated requests.
                    if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                        match method {
                            "session/update" => {
                                if self.handle_session_update(&msg) {
                                    let activity_now = Instant::now();
                                    idle_deadline = activity_now + idle_timeout;
                                    last_activity_at = activity_now;
                                    tracing::debug!("idle clock reset: tool call started");
                                }
                            }
                            "_goose/unstable/session/update" => {
                                self.handle_goose_usage_update(&msg);
                            }
                            "session/request_permission" => {
                                self.handle_permission_request(&msg).await?;
                            }
                            "elicitation/create" => {
                                self.handle_elicitation_request(&msg, elicitation_rx.is_some())
                                    .await?;
                            }
                            "$/cancel_request" => self.handle_cancel_request(&msg),
                            other => {
                                // If the unknown message has an id, it's a request expecting a reply.
                                // Silence would cause the agent to hang waiting for a response.
                                // Send a JSON-RPC -32601 "Method not found" error.
                                if msg.get("id").is_some() {
                                    let err_resp = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": msg["id"],
                                        "error": {"code": -32601, "message": format!("Method not found: {other}")}
                                    });
                                    // Surface write failures — a broken pipe means the
                                    // agent process is dead and continuing would hang.
                                    self.write_ndjson(&err_resp).await?;
                                }
                                tracing::debug!(target: "buzz_acp::acp::wire", "ignoring unknown method: {other}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Log a `session/update` notification via tracing.
    ///
    /// The discriminator field is `sessionUpdate` (not `type`) per the ACP schema.
    /// Returns `true` if the update indicates a tool call started, signaling that
    /// the idle clock should be explicitly reset (the agent will be silent while
    /// the tool executes).
    ///
    /// Takes `&mut self` (not `&self`) because some updates carry agent state
    /// the client must observe — notably goose's `session_info_update` with
    /// `_meta.goose.activeRunId`, which seeds [`active_run_id`](Self::active_run_id)
    /// so callers can target `_goose/unstable/session/steer` at the correct run.
    fn handle_session_update(&mut self, msg: &serde_json::Value) -> bool {
        let update = &msg["params"]["update"];
        let update_type = update
            .get("sessionUpdate")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        match update_type {
            "agent_message_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    tracing::info!(target: "buzz_acp::acp::stream", "{text}");
                }
                false
            }
            "tool_call" => {
                let title = update
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let kind = update
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                tracing::info!(target: "buzz_acp::acp::tool", "tool_call: {title} ({kind})");
                true
            }
            "tool_call_update" => {
                let tool_id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                tracing::info!(target: "buzz_acp::acp::tool", "tool_call_update: {tool_id} → {status}");
                false
            }
            "plan" => {
                tracing::info!(target: "buzz_acp::acp::plan", "plan update received");
                false
            }
            "agent_thought_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    tracing::debug!(target: "buzz_acp::acp::thought", "{text}");
                }
                false
            }
            "available_commands_update" => {
                // Advertised slash commands (ACP slash-commands extension).
                // Logged for observability; UI surfacing is a follow-up.
                let names: Vec<&str> = update["availableCommands"]
                    .as_array()
                    .map(|cmds| cmds.iter().filter_map(|c| c["name"].as_str()).collect())
                    .unwrap_or_default();
                tracing::info!(
                    target: "buzz_acp::acp::update",
                    "available_commands_update: {} commands [{}]",
                    names.len(),
                    names.join(", ")
                );
                false
            }
            "session_info_update" => {
                // Both goose and buzz-agent emit `session_info_update` with
                // `_meta.goose.activeRunId`: the id of the currently-active
                // prompt run, or `null` when the run has cleared. Other agents
                // don't emit this field; for them `active_run_id` stays `None`
                // and steer callers will fall back to cancel+merge.
                //
                // Per the ACP `SessionInfoUpdate` schema, `_meta` is a field
                // on the update object itself — nested inside `update`, not
                // alongside it at the params level. Goose and buzz-agent both
                // emit it at `params.update._meta.goose.activeRunId`.
                let meta = msg["params"]["update"]
                    .get("_meta")
                    .and_then(|m| m.get("goose"));
                if let Some(goose_meta) = meta {
                    match goose_meta.get("activeRunId") {
                        Some(serde_json::Value::String(run_id)) => {
                            tracing::debug!(
                                target: "buzz_acp::acp::update",
                                "session_info_update: activeRunId={run_id}"
                            );
                            self.active_run_id = Some(run_id.clone());
                        }
                        Some(serde_json::Value::Null) => {
                            tracing::debug!(
                                target: "buzz_acp::acp::update",
                                "session_info_update: activeRunId cleared"
                            );
                            self.active_run_id = None;
                        }
                        // Missing or non-string/null — leave state untouched.
                        _ => {}
                    }
                }
                false
            }
            "keepalive" => false,
            other => {
                tracing::debug!(target: "buzz_acp::acp::update", "session/update: {other}");
                false
            }
        }
    }

    /// Parse a `_goose/unstable/session/update` notification and record the
    /// usage snapshot in the per-session tracker.
    ///
    /// Silently ignores malformed or non-`usage_update` variants — the
    /// notification is best-effort observability data, not a protocol
    /// requirement. Failures are logged at debug level.
    fn handle_goose_usage_update(&mut self, msg: &serde_json::Value) {
        use crate::usage::{GooseSessionUpdateNotification, GooseSessionUpdateVariant};
        let params = match msg.get("params") {
            Some(p) => p,
            None => {
                tracing::debug!(
                    target: "buzz_acp::acp::usage",
                    "_goose/unstable/session/update: missing params"
                );
                return;
            }
        };
        match serde_json::from_value::<GooseSessionUpdateNotification>(params.clone()) {
            Ok(notif) => {
                if let GooseSessionUpdateVariant::UsageUpdate(payload) = &notif.update {
                    tracing::debug!(
                        target: "buzz_acp::acp::usage",
                        session_id = %notif.session_id,
                        input = payload.accumulated_input_tokens,
                        output = payload.accumulated_output_tokens,
                        "goose usage update"
                    );
                    self.goose_usage.record(&notif.session_id, payload);
                }
            }
            Err(e) => {
                tracing::debug!(
                    target: "buzz_acp::acp::usage",
                    "_goose/unstable/session/update: deserialization error: {e}"
                );
            }
        }
    }

    /// Auto-approve a `session/request_permission` request from the agent.
    ///
    /// Finds the option with `kind == "allow_once"` and responds with its `optionId`.
    /// If no `allow_once` option exists, falls back to `reject_once`.
    ///
    /// **Critical:** Never hardcode `optionId` — always find it dynamically by `kind`.
    ///
    /// The request `id` is stored as `serde_json::Value` to support both numeric
    /// and string IDs per JSON-RPC 2.0.
    async fn handle_permission_request(&mut self, msg: &serde_json::Value) -> Result<(), AcpError> {
        // Extract id as a Value — JSON-RPC 2.0 allows both numeric and string IDs.
        let id = msg
            .get("id")
            .cloned()
            .ok_or_else(|| AcpError::Protocol("permission request missing id".into()))?;

        // Store pending permission id so cancel_with_cleanup can respond to it.
        self.pending_permission_id = Some(id.clone());
        // Mark as not yet responded — guards against double-response race.
        self.permission_responded = false;

        let options = msg["params"]["options"]
            .as_array()
            .ok_or_else(|| AcpError::Protocol("permission request missing options".into()))?;

        tracing::debug!(
            target: "buzz_acp::acp::permission",
            "session/request_permission id={id}, {} options",
            options.len()
        );

        // Find allow_once by kind — NEVER hardcode optionId.
        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));

        let response = if let Some(opt) = allow_once {
            let option_id = opt["optionId"]
                .as_str()
                .ok_or_else(|| AcpError::Protocol("allow_once option missing optionId".into()))?;
            tracing::info!(
                target: "buzz_acp::acp::permission",
                "auto-approving permission id={id} with allow_once optionId={option_id:?}"
            );
            permission_response_selected(&id, option_id)
        } else {
            // No allow_once — fall back to reject_once.
            tracing::warn!(
                target: "buzz_acp::acp::permission",
                "no allow_once option found in permission request id={id}, falling back to reject_once"
            );
            let reject = options
                .iter()
                .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("reject_once"));

            if let Some(opt) = reject {
                let option_id = opt["optionId"].as_str().unwrap_or("reject");
                permission_response_selected(&id, option_id)
            } else {
                return Err(AcpError::Protocol(
                    "no suitable permission option found (neither allow_once nor reject_once)"
                        .into(),
                ));
            }
        };

        // Write the response first, then mark as responded.
        //
        // Previous ordering (flag-before-write) was intended to guard against a
        // double-response if a timeout fires between write and flag-set. However,
        // the deadlock risk is worse: if write_ndjson fails (e.g. WriteTimeout),
        // the flag would be true but no response was actually sent. Then
        // cancel_with_cleanup would see permission_responded=true, skip sending
        // the cancelled outcome, and the agent would hang waiting for a reply
        // that never arrives — a guaranteed deadlock.
        //
        // The correct fix: set the flag AFTER a successful write. The double-
        // response window (between write completion and flag-set) is negligibly
        // small and bounded by a single memory store; the deadlock window was
        // unbounded.
        self.write_ndjson(&response).await?;
        self.permission_responded = true;
        self.pending_permission_id = None;
        Ok(())
    }

    /// Ask the channel owner an `elicitation/create` form question.
    ///
    /// Publishes the first field as a channel message and parks the request;
    /// the read loop's elicitation arm folds replies in and answers when the
    /// last field is done. Requests we cannot render — a non-form mode, an
    /// empty form, a turn with no channel to publish into or no live reply
    /// channel to answer on (`answerable`), or a second request while one is
    /// already parked — are answered `cancel` here, which the agent surfaces
    /// as an aborted tool call rather than a hang.
    async fn handle_elicitation_request(
        &mut self,
        msg: &serde_json::Value,
        answerable: bool,
    ) -> Result<(), AcpError> {
        let id = msg
            .get("id")
            .cloned()
            .ok_or_else(|| AcpError::Protocol("elicitation request missing id".into()))?;

        let fields = parse_elicitation_fields(&msg["params"]).filter(|_| {
            answerable && self.elicitation.is_some() && self.pending_elicitation.is_none()
        });
        let Some(fields) = fields else {
            tracing::warn!(
                target: "buzz_acp::acp::elicitation",
                "cancelling unanswerable elicitation id={id}"
            );
            return self
                .write_ndjson(&elicitation_response(&id, "cancel", None))
                .await;
        };

        tracing::info!(
            target: "buzz_acp::acp::elicitation",
            "asking owner {} question(s) for elicitation id={id}",
            fields.len()
        );
        self.pending_elicitation = Some(PendingElicitation {
            id,
            fields,
            asking: 0,
            answers: serde_json::Map::new(),
        });
        self.ask_parked_elicitation().await
    }

    /// Fold an owner reply into the parked elicitation, answering the agent
    /// once every field has been asked.
    async fn apply_elicitation_reply(
        &mut self,
        reply: crate::pool::ElicitationReply,
    ) -> Result<(), AcpError> {
        let Some(pending) = self.pending_elicitation.as_mut() else {
            return Ok(());
        };
        let response = match reply {
            crate::pool::ElicitationReply::Skip => {
                tracing::info!(
                    target: "buzz_acp::acp::elicitation",
                    "owner skipped elicitation id={}", pending.id
                );
                elicitation_response(&pending.id, "decline", None)
            }
            crate::pool::ElicitationReply::Answer(text) => {
                answer_elicitation_field(
                    &pending.fields[pending.asking],
                    &text,
                    &mut pending.answers,
                );
                pending.asking += 1;
                if pending.asking < pending.fields.len() {
                    return self.ask_parked_elicitation().await;
                }
                let answers = std::mem::take(&mut pending.answers);
                elicitation_response(&pending.id, "accept", Some(answers))
            }
        };
        // Write before dropping the parked request, for the reason documented on
        // `handle_permission_request`: a failed write must leave the agent
        // answerable by teardown rather than waiting forever.
        self.write_ndjson(&response).await?;
        self.take_pending_elicitation();
        Ok(())
    }

    /// Drop a parked elicitation the agent has cancelled.
    ///
    /// `$/cancel_request` is a notification naming a request the peer has
    /// abandoned; per JSON-RPC that request is gone, so we must not respond to
    /// it. Cancellations for anything else are ignored — the read loop's own
    /// deadlines bound every other request we serve.
    fn handle_cancel_request(&mut self, msg: &serde_json::Value) {
        let Some(parked) = self.pending_elicitation.as_ref() else {
            return;
        };
        if msg["params"].get("requestId") == Some(&parked.id) {
            tracing::info!(target: "buzz_acp::acp::elicitation", "agent cancelled elicitation id={}", parked.id);
            self.take_pending_elicitation();
        }
    }

    /// Parse `stopReason` from a `session/prompt` result value.
    fn parse_stop_reason(&self, result: &serde_json::Value) -> Result<StopReason, AcpError> {
        let raw = result["stopReason"].as_str().ok_or_else(|| {
            AcpError::Protocol("session/prompt response missing stopReason".into())
        })?;
        StopReason::from_str(raw)
            .ok_or_else(|| AcpError::Protocol(format!("unknown stopReason: {raw:?}")))
    }
}

/// Build `session/prompt` params from one or more text content blocks.
fn build_prompt_params(session_id: &str, prompt_blocks: &[&str]) -> serde_json::Value {
    let blocks: Vec<serde_json::Value> = prompt_blocks
        .iter()
        .map(|text| serde_json::json!({ "type": "text", "text": text }))
        .collect();
    serde_json::json!({
        "sessionId": session_id,
        "prompt": blocks,
    })
}

/// Build `_goose/unstable/session/steer` params from one or more text
/// content blocks plus the freshest `expectedRunId`.
///
/// Wire shape:
/// ```json
/// { "sessionId": "...", "expectedRunId": "...", "prompt": [{"type":"text","text":"..."}, ...] }
/// ```
///
/// Called from the read-loop steer arm at write time so `expectedRunId`
/// matches goose's *current* run (it advances on each `session/update`).
/// See [`crate::pool::SteerRequest`] for why this is the read loop's job
/// and not the main loop's.
fn build_steer_params(
    session_id: &str,
    expected_run_id: &str,
    prompt_blocks: &[&str],
) -> serde_json::Value {
    let blocks: Vec<serde_json::Value> = prompt_blocks
        .iter()
        .map(|text| serde_json::json!({ "type": "text", "text": text }))
        .collect();
    serde_json::json!({
        "sessionId": session_id,
        "expectedRunId": expected_run_id,
        "prompt": blocks,
    })
}

/// Build a JSON-RPC permission response with `outcome: "selected"`.
fn permission_response_selected(id: &serde_json::Value, option_id: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": { "outcome": "selected", "optionId": option_id } }
    })
}

/// Build a JSON-RPC permission response with `outcome: "cancelled"`.
fn permission_response_cancelled(id: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": { "outcome": "cancelled" } }
    })
}

/// Cap on the serialized `ask` tag payload. A question whose structure exceeds
/// this is published body-only: the numbered list is already answerable, so a
/// card is worth no risk to the event's size.
const ASK_TAG_MAX_BYTES: usize = 4096;

/// One selectable answer to an elicitation form field.
struct ElicitationOption {
    /// The value written back in the response.
    value: String,
    /// Human-readable label; equals `value` for untitled enums.
    title: String,
    description: Option<String>,
}

/// One question from an elicitation form's `requestedSchema`.
struct ElicitationField {
    /// Property key the answer is written back under.
    name: String,
    /// JSON Schema `type` — drives coercion of the owner's plain-text reply.
    ty: String,
    /// The free-text sibling property (`<name>_custom`), when the form pairs
    /// one with this field. Holds answers that name no option.
    custom: Option<String>,
    /// The question text shown to the owner: the schema `description`, else the
    /// request `message`. Never the schema `title` — adapters put a short chip
    /// label there ("Library"), not the question.
    prompt: String,
    /// Empty for free-text fields.
    options: Vec<ElicitationOption>,
}

impl ElicitationField {
    /// Match one reply token against this field's options: a 1-based index, or
    /// an option value/title compared case-insensitively.
    fn select(&self, token: &str) -> Option<&ElicitationOption> {
        let token = token.trim();
        if let Some(option) = token
            .parse::<usize>()
            .ok()
            .and_then(|index| self.options.get(index.checked_sub(1)?))
        {
            return Some(option);
        }
        self.options
            .iter()
            .find(|o| o.value.eq_ignore_ascii_case(token) || o.title.eq_ignore_ascii_case(token))
    }

    /// Whether an answer naming no option still reaches the agent — the single
    /// source of truth behind the `ask` tag's `allowFreeText`, and therefore
    /// behind the card's "answer in your own words" box.
    ///
    /// This mirrors what [`answer_elicitation_field`] actually does rather than
    /// what the schema declares. Adapters (codex, claude) routinely send a bare
    /// single-select — an `enum`/`oneOf` with no `<name>_custom` sibling — and
    /// the harness has always accepted free text for it: `select` returns
    /// `None` for a reply matching no option, so the reply falls through to
    /// [`coerce_elicitation_answer`] and the agent receives the owner's words
    /// verbatim under the field's own key. [`render_elicitation_field`] has
    /// always advertised exactly that ("Reply with the number, your own answer,
    /// or `!skip`"); only the card withheld the box, so the owner's one
    /// affordance for "none of these" was to ignore the card and type into the
    /// thread.
    ///
    /// Multi-select is the one shape left alone. A free-text reply to an array
    /// field is split on `,` by [`coerce_elicitation_answer`] into whatever
    /// tokens it contains, so the agent gets a list of arbitrary strings where
    /// it asked for a list of enum members — a worse answer than the numbered
    /// body already collects. It stays opt-in via a `<name>_custom` sibling.
    fn accepts_free_text(&self) -> bool {
        // No options at all: free text is the only possible answer.
        if self.options.is_empty() {
            return true;
        }
        // A paired `<name>_custom` sibling is the adapter asking for one.
        if self.custom.is_some() {
            return true;
        }
        self.ty != "array"
    }
}

/// An `elicitation/create` request parked awaiting owner replies.
struct PendingElicitation {
    /// Stored as a `serde_json::Value` because JSON-RPC 2.0 permits both
    /// numeric and string IDs from the agent.
    id: serde_json::Value,
    fields: Vec<ElicitationField>,
    /// Index into `fields` of the question currently published.
    asking: usize,
    /// Answers gathered so far — becomes the `content` of the accept response.
    answers: serde_json::Map<String, serde_json::Value>,
}

/// Flatten an elicitation form into the questions to ask, in schema key order
/// (lexicographic — this workspace builds `serde_json` without
/// `preserve_order`; the keys are `question_<n>`, so it reads naturally).
/// Returns `None` for a non-form request or a form with no askable field.
///
/// A `<name>_custom` property whose sibling `<name>` also exists is the
/// per-question free-text box adapters pair with a select field, not a question
/// of its own — it holds an answer that names no option.
fn parse_elicitation_fields(params: &serde_json::Value) -> Option<Vec<ElicitationField>> {
    if params.get("mode").and_then(|m| m.as_str()) != Some("form") {
        return None;
    }
    let properties = params["requestedSchema"]["properties"].as_object()?;
    let message = params["message"].as_str().unwrap_or_default();
    let fields: Vec<ElicitationField> = properties
        .iter()
        .filter(|(name, _)| {
            !name
                .strip_suffix("_custom")
                .is_some_and(|base| properties.contains_key(base))
        })
        .map(|(name, schema)| ElicitationField {
            custom: Some(format!("{name}_custom")).filter(|key| properties.contains_key(key)),
            prompt: schema["description"].as_str().unwrap_or(message).to_owned(),
            options: elicitation_options(schema),
            ty: schema["type"].as_str().unwrap_or("string").to_owned(),
            name: name.clone(),
        })
        .collect();
    (!fields.is_empty()).then_some(fields)
}

/// Selectable values for a field: titled `oneOf`/`anyOf` options or a bare
/// `enum`, read off the field itself or — for `type: "array"` — off its `items`.
fn elicitation_options(schema: &serde_json::Value) -> Vec<ElicitationOption> {
    let source = if schema["type"] == "array" {
        &schema["items"]
    } else {
        schema
    };
    if let Some(list) = source["oneOf"]
        .as_array()
        .or_else(|| source["anyOf"].as_array())
    {
        return list
            .iter()
            .filter_map(|option| {
                let value = option["const"].as_str()?.to_owned();
                Some(ElicitationOption {
                    title: option["title"].as_str().unwrap_or(&value).to_owned(),
                    description: option["description"].as_str().map(str::to_owned),
                    value,
                })
            })
            .collect();
    }
    source["enum"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(|value| ElicitationOption {
            value: value.to_owned(),
            title: value.to_owned(),
            description: None,
        })
        .collect()
}

/// Serialize one question's structure for the kind:9's `ask` tag, so a richer
/// client can render it as a card instead of the numbered prose below it.
///
/// The tag — not a content marker — carries the machine-readable signal:
/// a marker in the body renders literally on every client that doesn't know it
/// (react-markdown turns an unrecognized HTML comment into a text node), while
/// an unknown tag is simply ignored. The body stays the fallback contract:
/// clients that ignore the tag show the same answerable numbered list they
/// always did.
///
/// `label` is what a card sends back — `ElicitationField::select` resolves an
/// option title case-insensitively, so a click needs no harness change.
/// Returns `None` when the payload would exceed [`ASK_TAG_MAX_BYTES`].
fn elicitation_ask_tag(
    field: &ElicitationField,
    index: usize,
    total: usize,
) -> Option<Vec<String>> {
    let payload = serde_json::json!({
        "v": 1,
        "question": field.prompt,
        "options": field
            .options
            .iter()
            .map(|option| {
                let mut entry = serde_json::json!({ "label": option.title });
                if let Some(description) = &option.description {
                    entry["description"] = serde_json::json!(description);
                }
                entry
            })
            .collect::<Vec<_>>(),
        "multiSelect": field.ty == "array",
        // Whether the answer path accepts words that name no option — see
        // `ElicitationField::accepts_free_text`, which is deliberately wider
        // than the schema: a bare single-select has always taken free text.
        "allowFreeText": field.accepts_free_text(),
        "index": index,
        "total": total,
    });
    let json = serde_json::to_string(&payload).ok()?;
    (json.len() <= ASK_TAG_MAX_BYTES).then(|| vec!["ask".to_owned(), json])
}

/// Render one question as the channel message body the owner answers.
fn render_elicitation_field(field: &ElicitationField, index: usize, total: usize) -> String {
    use std::fmt::Write;

    let mut body = String::new();
    if total > 1 {
        let _ = writeln!(body, "_Question {} of {total}_", index + 1);
    }
    let _ = write!(body, "**{}**", field.prompt);
    for (position, option) in field.options.iter().enumerate() {
        let _ = write!(body, "\n{}. {}", position + 1, option.title);
        if let Some(description) = &option.description {
            let _ = write!(body, " — {description}");
        }
    }
    body.push_str(if field.options.is_empty() {
        "\n\nReply with your answer, or `!skip`."
    } else if field.ty == "array" {
        "\n\nReply with the numbers (comma-separated), your own answer, or `!skip`."
    } else {
        "\n\nReply with the number, your own answer, or `!skip`."
    });
    body
}

/// Fold the owner's plain-text `reply` into `answers` under `field`'s key, or
/// under its free-text sibling when the reply names no option.
fn answer_elicitation_field(
    field: &ElicitationField,
    reply: &str,
    answers: &mut serde_json::Map<String, serde_json::Value>,
) {
    let reply = reply.trim();
    let selected: Vec<&ElicitationOption> = if field.ty == "array" {
        reply
            .split(',')
            .filter_map(|token| field.select(token))
            .collect()
    } else {
        field.select(reply).into_iter().collect()
    };
    if let Some(key) = field.custom.clone().filter(|_| selected.is_empty()) {
        answers.insert(key, serde_json::json!(reply));
    } else if selected.is_empty() {
        answers.insert(
            field.name.clone(),
            coerce_elicitation_answer(&field.ty, reply),
        );
    } else if field.ty == "array" {
        let values: Vec<&str> = selected.iter().map(|o| o.value.as_str()).collect();
        answers.insert(field.name.clone(), serde_json::json!(values));
    } else {
        answers.insert(field.name.clone(), serde_json::json!(selected[0].value));
    }
}

/// Coerce a plain-text reply to the JSON type the form asked for, falling back
/// to the raw string when it doesn't parse — losing the owner's words is worse
/// than handing the agent a loosely-typed answer.
fn coerce_elicitation_answer(ty: &str, reply: &str) -> serde_json::Value {
    match ty {
        "boolean" => match reply.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" => serde_json::json!(true),
            "n" | "no" | "false" => serde_json::json!(false),
            _ => serde_json::json!(reply),
        },
        "integer" => reply
            .parse::<i64>()
            .map_or_else(|_| serde_json::json!(reply), |n| serde_json::json!(n)),
        "number" => reply
            .parse::<f64>()
            .map_or_else(|_| serde_json::json!(reply), |n| serde_json::json!(n)),
        "array" => serde_json::json!(reply.split(',').map(str::trim).collect::<Vec<_>>()),
        _ => serde_json::json!(reply),
    }
}

/// Build a JSON-RPC `elicitation/create` response. `content` is only ever
/// carried by `accept`; `decline` tells the agent the owner skipped (the turn
/// continues) and `cancel` aborts the asking tool call.
fn elicitation_response(
    id: &serde_json::Value,
    action: &str,
    content: Option<serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    let mut result = serde_json::json!({ "action": action });
    if let Some(content) = content {
        result["content"] = serde_json::Value::Object(content);
    }
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Full `session/new` response — session ID plus the raw JSON result.
///
/// Callers use the extractor helpers to pull model info from `raw`.
pub struct SessionNewResponse {
    pub session_id: String,
    /// The full `result` value from the JSON-RPC response.
    pub raw: serde_json::Value,
}

/// How to switch to a particular model on a session.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type")]
pub enum ModelSwitchMethod {
    /// Stable: use `session/set_config_option` with these exact values.
    ConfigOption {
        config_id: String,
        option_value: String,
    },
    /// Unstable: use `session/set_model` with this model_id.
    SetModel { model_id: String },
}

/// Extract `configOptions` entries with `category == "model"` from a `session/new` result.
///
/// Returns the raw JSON array entries. Each entry has `id`, `name`,
/// `options: [{ value, name }]`, etc.
pub fn extract_model_config_options(result: &serde_json::Value) -> Vec<serde_json::Value> {
    result["configOptions"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|opt| opt.get("category").and_then(|c| c.as_str()) == Some("model"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Identifier of a `configOptions` entry.
///
/// Responses carry `id` (`SessionConfigOption.id`); `configId` is request-only
/// (`SetSessionConfigOptionRequest.configId`). Tolerate the latter for adapters
/// that echo the request shape back.
pub fn config_option_id(config_opt: &serde_json::Value) -> Option<&str> {
    config_opt
        .get("id")
        .or_else(|| config_opt.get("configId"))
        .and_then(|v| v.as_str())
}

/// Human-readable label of a `configOptions` entry or one of its options.
///
/// The schema field is `name`; `displayName` is a pre-standardization spelling
/// still emitted by some adapters.
pub fn config_option_label(value: &serde_json::Value) -> Option<&str> {
    value
        .get("name")
        .or_else(|| value.get("displayName"))
        .and_then(|v| v.as_str())
}

/// Extract `SessionModelState` (unstable path) from a `session/new` result.
///
/// Returns the `models` object if present: `{ currentModelId, availableModels: [...] }`.
pub fn extract_model_state(result: &serde_json::Value) -> Option<serde_json::Value> {
    result.get("models").cloned()
}

/// Match a desired model ID against a fresh `session/new` response.
///
/// Returns the correct ACP method to call, or `None` if no match.
///
/// **Precedence**: stable `configOptions` first (spec-blessed), then unstable
/// `availableModels`. The fresh `session/new` response is always authoritative.
pub fn resolve_model_switch_method(
    session_new_result: &serde_json::Value,
    desired_model: &str,
) -> Option<ModelSwitchMethod> {
    // 1. Search stable configOptions for a "model"-category entry whose
    //    options contain a value matching desired_model.
    for config_opt in extract_model_config_options(session_new_result) {
        let config_id = match config_option_id(&config_opt) {
            Some(id) => id,
            None => continue,
        };
        if let Some(options) = config_opt.get("options").and_then(|v| v.as_array()) {
            for opt in options {
                if opt.get("value").and_then(|v| v.as_str()) == Some(desired_model) {
                    return Some(ModelSwitchMethod::ConfigOption {
                        config_id: config_id.to_string(),
                        option_value: desired_model.to_string(),
                    });
                }
            }
        }
    }

    // 2. Search unstable availableModels for a matching modelId.
    if let Some(models) = extract_model_state(session_new_result) {
        if let Some(available) = models.get("availableModels").and_then(|v| v.as_array()) {
            for model in available {
                if model.get("modelId").and_then(|v| v.as_str()) == Some(desired_model) {
                    return Some(ModelSwitchMethod::SetModel {
                        model_id: desired_model.to_string(),
                    });
                }
            }
        }
    }

    // 3. No match.
    None
}

/// Whether `desired_model` appears in pre-extracted catalog halves.
///
/// Mirrors [`resolve_model_switch_method`]'s match, but operates on the
/// already-extracted `configOptions` (model category) and `models` state that
/// [`AgentModelCapabilities`](crate::pool::AgentModelCapabilities) caches — the
/// idle-path pre-cancel guard has those halves, not the full `session/new` JSON.
pub fn model_in_catalog(
    config_options: &[serde_json::Value],
    available_models: Option<&serde_json::Value>,
    desired_model: &str,
) -> bool {
    let in_config_options = config_options.iter().any(|config_opt| {
        config_opt
            .get("options")
            .and_then(|v| v.as_array())
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|opt| opt.get("value").and_then(|v| v.as_str()) == Some(desired_model))
            })
    });
    if in_config_options {
        return true;
    }

    available_models
        .and_then(|models| models.get("availableModels"))
        .and_then(|v| v.as_array())
        .is_some_and(|available| {
            available
                .iter()
                .any(|model| model.get("modelId").and_then(|v| v.as_str()) == Some(desired_model))
        })
}

// ─── Drop: kill child process ─────────────────────────────────────────────────

impl Drop for AcpClient {
    fn drop(&mut self) {
        // Best-effort SIGKILL + reap. We cannot `await` in Drop (sync context).
        // Kill the process group when possible so subprocesses don't leak.
        // Callers SHOULD still call `shutdown().await` for guaranteed reaping.
        match self.child.id() {
            Some(pid) if kill_process_group(pid) => {}
            _ => {
                let _ = self.child.start_kill();
            }
        }
        // Non-blocking reap attempt — prevents zombie accumulation in the
        // common case where SIGKILL takes effect before Drop returns.
        let _ = self.child.try_wait();
    }
}

/// Send SIGKILL to an entire process group. Returns `true` if the signal was sent.
///
/// The child is spawned with `process_group(0)`, so its PID equals its PGID.
/// Killing the group ensures subprocesses (MCP servers, tool processes) are
/// cleaned up rather than orphaned to init on repeated crash-recovery cycles.
///
/// Uses `nix::sys::signal::killpg` — a safe wrapper around the POSIX `killpg`
/// syscall — so the crate's `#![deny(unsafe_code)]` policy is preserved.
#[cfg(unix)]
fn kill_process_group(pid: u32) -> bool {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    // pid == pgid because the child was spawned with process_group(0).
    killpg(Pid::from_raw(pid as i32), Signal::SIGKILL).is_ok()
}

/// Fallback for non-Unix: process-group kill not available.
/// Returns `false` so the caller falls back to `child.start_kill()`.
#[cfg(not(unix))]
fn kill_process_group(_pid: u32) -> bool {
    false
}

/// Suppress the console window that Windows otherwise allocates for every
/// console-subsystem child process spawned from a GUI (non-console) parent.
/// No-op on non-Windows platforms.
fn configure_no_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_parses_all_known_values() {
        assert_eq!(StopReason::from_str("end_turn"), Some(StopReason::EndTurn));
        assert_eq!(
            StopReason::from_str("cancelled"),
            Some(StopReason::Cancelled)
        );
        assert_eq!(
            StopReason::from_str("max_tokens"),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            StopReason::from_str("max_turn_requests"),
            Some(StopReason::MaxTurnRequests)
        );
        assert_eq!(StopReason::from_str("refusal"), Some(StopReason::Refusal));
    }

    #[test]
    fn stop_reason_returns_none_for_unknown() {
        assert_eq!(StopReason::from_str("unknown_value"), None);
        assert_eq!(StopReason::from_str(""), None);
        assert_eq!(StopReason::from_str("endturn"), None); // no camelCase — still unknown
    }

    #[test]
    fn stop_reason_is_case_insensitive() {
        // Agents may send uppercase or mixed-case variants — all should parse correctly.
        assert_eq!(StopReason::from_str("END_TURN"), Some(StopReason::EndTurn));
        assert_eq!(
            StopReason::from_str("CANCELLED"),
            Some(StopReason::Cancelled)
        );
        assert_eq!(
            StopReason::from_str("Max_Tokens"),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            StopReason::from_str("MAX_TURN_REQUESTS"),
            Some(StopReason::MaxTurnRequests)
        );
        assert_eq!(StopReason::from_str("Refusal"), Some(StopReason::Refusal));
    }

    #[test]
    fn find_allow_once_by_kind_not_by_option_id() {
        // optionId values are intentionally non-obvious to prove we don't hardcode them.
        let options: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
            {"optionId": "opt-reject-42",  "name": "Reject",       "kind": "reject_once"},
            {"optionId": "opt-allow-99",   "name": "Allow once",   "kind": "allow_once"},
            {"optionId": "opt-always-7",   "name": "Always allow", "kind": "allow_always"}
        ]"#,
        )
        .unwrap();

        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));

        assert!(allow_once.is_some(), "should find allow_once option");
        let opt = allow_once.unwrap();
        // Found by kind, not by hardcoded optionId
        assert_eq!(opt["kind"].as_str(), Some("allow_once"));
        assert_eq!(opt["optionId"].as_str(), Some("opt-allow-99"));
    }

    #[test]
    fn find_allow_once_returns_none_when_absent() {
        let options: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
            {"optionId": "reject-1",      "name": "Reject",        "kind": "reject_once"},
            {"optionId": "reject-always", "name": "Always reject", "kind": "reject_always"}
        ]"#,
        )
        .unwrap();

        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));

        assert!(allow_once.is_none());
    }

    #[test]
    fn find_reject_once_fallback_when_no_allow_once() {
        let options: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"optionId": "rej-x", "name": "Reject", "kind": "reject_once"}]"#,
        )
        .unwrap();

        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));
        assert!(allow_once.is_none());

        let reject_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("reject_once"));
        assert!(reject_once.is_some());
        assert_eq!(reject_once.unwrap()["optionId"].as_str(), Some("rej-x"));
    }

    #[test]
    fn request_has_id_field() {
        let id: u64 = 42;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {}
        });
        assert!(msg.get("id").is_some(), "request must have id field");
        assert_eq!(msg["id"].as_u64(), Some(42));
        assert_eq!(msg["jsonrpc"].as_str(), Some("2.0"));
        assert_eq!(msg["method"].as_str(), Some("initialize"));
    }

    #[test]
    fn notification_has_no_id_field() {
        // session/cancel is a notification — must NOT have an id field.
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {
                "sessionId": "sess_abc123"
            }
        });
        assert!(
            msg.get("id").is_none(),
            "notification must NOT have id field"
        );
        assert_eq!(msg["jsonrpc"].as_str(), Some("2.0"));
        assert_eq!(msg["method"].as_str(), Some("session/cancel"));
    }

    #[test]
    fn initialize_request_format() {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0u64,
            "method": "initialize",
            "params": {
                "protocolVersion": 2,
                "clientCapabilities": build_client_capabilities(),
                "clientInfo": {
                    "name": "buzz-acp",
                    "version": "0.1.0"
                }
            }
        });
        assert_eq!(msg["params"]["protocolVersion"].as_u64(), Some(2));
        assert_eq!(
            msg["params"]["clientInfo"]["name"].as_str(),
            Some("buzz-acp")
        );
        assert!(msg["params"]["clientCapabilities"].is_object());
        assert_eq!(
            msg["params"]["clientCapabilities"]["auth"]["terminal"].as_bool(),
            Some(true),
            "terminal auth capability must be advertised so adapters can expose terminal login methods"
        );
        assert_eq!(
            msg["params"]["clientCapabilities"]["_meta"]["goose"]["customNotifications"].as_bool(),
            Some(true),
            "goose customNotifications capability must be advertised"
        );
        let elicitation = &msg["params"]["clientCapabilities"]["elicitation"];
        assert_eq!(
            elicitation["form"],
            serde_json::json!({}),
            "form elicitation must be advertised or adapters strip AskUserQuestion"
        );
        assert!(
            elicitation.get("url").is_none(),
            "url elicitation must stay undeclared — there is no browser-handoff surface"
        );
    }

    #[test]
    fn session_new_mcp_server_has_required_fields() {
        // Schema requires name, command, args, env — all present, args/env may be empty.
        let server = McpServer {
            name: "test-mcp".into(),
            command: "/usr/local/bin/test-mcp-server".into(),
            args: vec![],
            env: vec![
                EnvVar {
                    name: "BUZZ_RELAY_URL".into(),
                    value: "ws://localhost:3000".into(),
                },
                EnvVar {
                    name: "BUZZ_PRIVATE_KEY".into(),
                    value: "nsec1abc".into(),
                },
            ],
        };
        let serialized = serde_json::to_value(&server).unwrap();
        assert_eq!(serialized["name"].as_str(), Some("test-mcp"));
        assert_eq!(
            serialized["command"].as_str(),
            Some("/usr/local/bin/test-mcp-server")
        );
        assert!(serialized["args"].is_array());
        assert_eq!(serialized["args"].as_array().unwrap().len(), 0);
        assert!(serialized["env"].is_array());
        assert_eq!(serialized["env"].as_array().unwrap().len(), 2);
        assert_eq!(
            serialized["env"][0]["name"].as_str(),
            Some("BUZZ_RELAY_URL")
        );
    }

    #[test]
    fn session_prompt_request_format() {
        let prompt_text = "[Buzz @mention]\nChannel: test\nFrom: npub1...\nMessage: hello";
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2u64,
            "method": "session/prompt",
            "params": {
                "sessionId": "sess_abc123",
                "prompt": [
                    { "type": "text", "text": prompt_text }
                ]
            }
        });
        assert_eq!(msg["method"].as_str(), Some("session/prompt"));
        let prompt = msg["params"]["prompt"].as_array().unwrap();
        assert_eq!(prompt.len(), 1);
        assert_eq!(prompt[0]["type"].as_str(), Some("text"));
        assert_eq!(prompt[0]["text"].as_str(), Some(prompt_text));
    }

    #[test]
    fn session_prompt_slash_command_two_block_format() {
        // Slash-command pass-through: bare command first, wrapped context second.
        let params = build_prompt_params(
            "sess_abc123",
            &[
                "/goal ship it",
                "[Buzz event: @mention]\nContent: @Eva /goal ship it",
            ],
        );
        let prompt = params["prompt"].as_array().unwrap();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0]["type"].as_str(), Some("text"));
        assert_eq!(prompt[0]["text"].as_str(), Some("/goal ship it"));
        assert!(prompt[0]["text"].as_str().unwrap().starts_with('/'));
        assert_eq!(prompt[1]["type"].as_str(), Some("text"));
    }

    #[test]
    fn permission_response_selected_format() {
        let id: u64 = 5;
        let option_id = "opt-allow-99";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id
                }
            }
        });
        assert_eq!(response["id"].as_u64(), Some(5));
        assert_eq!(
            response["result"]["outcome"]["outcome"].as_str(),
            Some("selected")
        );
        assert_eq!(
            response["result"]["outcome"]["optionId"].as_str(),
            Some("opt-allow-99")
        );
    }

    #[test]
    fn permission_response_cancelled_format() {
        let id: u64 = 5;
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "outcome": {
                    "outcome": "cancelled"
                }
            }
        });
        assert_eq!(
            response["result"]["outcome"]["outcome"].as_str(),
            Some("cancelled")
        );
        // cancelled outcome has no optionId
        assert!(response["result"]["outcome"].get("optionId").is_none());
    }

    #[test]
    fn session_cancel_notification_has_session_id_in_params() {
        let session_id = "sess_xyz789";
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {
                "sessionId": session_id
            }
        });
        // Must have no id (notification)
        assert!(msg.get("id").is_none());
        // Must have sessionId in params
        assert_eq!(msg["params"]["sessionId"].as_str(), Some("sess_xyz789"));
    }

    #[test]
    fn permission_request_with_string_id() {
        // Verify that permission response uses the same ID type as the request.
        // JSON-RPC 2.0 permits string IDs from the agent.
        let string_id = serde_json::json!("perm-req-001");
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": string_id,
            "result": {
                "outcome": { "outcome": "selected", "optionId": "allow-once" }
            }
        });
        assert_eq!(response["id"], "perm-req-001");
        assert!(response["id"].is_string());
    }

    #[test]
    fn id_comparison_works_for_numeric_and_string() {
        // Verify json!(expected_id) comparison logic used in read_until_response.
        let expected_id: u64 = 3;
        let numeric_response_id = serde_json::json!(3u64);
        let string_response_id = serde_json::json!("3");

        // Numeric matches
        assert_eq!(numeric_response_id, serde_json::json!(expected_id));
        // String does NOT match numeric (correct — different types)
        assert_ne!(string_response_id, serde_json::json!(expected_id));
    }

    #[test]
    fn permission_cancelled_response_preserves_id_type() {
        // String ID from agent should be echoed back as string in cancelled response.
        let string_id = serde_json::json!("req-abc");
        let cancelled = serde_json::json!({
            "jsonrpc": "2.0",
            "id": string_id.clone(),
            "result": { "outcome": { "outcome": "cancelled" } }
        });
        assert_eq!(cancelled["id"], string_id);
        assert!(cancelled["id"].is_string());

        // Numeric ID from agent should be echoed back as numeric.
        let numeric_id = serde_json::json!(42u64);
        let cancelled_numeric = serde_json::json!({
            "jsonrpc": "2.0",
            "id": numeric_id.clone(),
            "result": { "outcome": { "outcome": "cancelled" } }
        });
        assert_eq!(cancelled_numeric["id"], numeric_id);
        assert!(cancelled_numeric["id"].is_number());
    }

    #[test]
    fn extract_model_config_options_finds_model_category() {
        let result = serde_json::json!({
            "sessionId": "sess-1",
            "configOptions": [
                {
                    "id": "model",
                    "category": "model",
                    "name": "Model",
                    "options": [
                        { "value": "claude-sonnet-4-20250514", "name": "Claude Sonnet 4" },
                        { "value": "claude-opus-4-20250514", "name": "Claude Opus 4" }
                    ]
                },
                {
                    "id": "theme",
                    "category": "appearance",
                    "name": "Theme",
                    "options": [{ "value": "dark", "name": "Dark" }]
                }
            ]
        });
        let opts = super::extract_model_config_options(&result);
        assert_eq!(opts.len(), 1);
        assert_eq!(super::config_option_id(&opts[0]), Some("model"));
    }

    #[test]
    fn extract_model_config_options_empty_when_no_config_options() {
        let result = serde_json::json!({ "sessionId": "sess-1" });
        assert!(super::extract_model_config_options(&result).is_empty());
    }

    #[test]
    fn extract_model_config_options_empty_when_no_model_category() {
        let result = serde_json::json!({
            "configOptions": [
                { "id": "theme", "category": "appearance" }
            ]
        });
        assert!(super::extract_model_config_options(&result).is_empty());
    }

    #[test]
    fn extract_model_state_returns_models_object() {
        let result = serde_json::json!({
            "sessionId": "sess-1",
            "models": {
                "currentModelId": "gpt-5",
                "availableModels": [
                    { "modelId": "gpt-5", "name": "GPT-5" },
                    { "modelId": "o3-pro", "name": "o3 Pro" }
                ]
            }
        });
        let ms = super::extract_model_state(&result).expect("should have models");
        assert_eq!(ms["currentModelId"].as_str(), Some("gpt-5"));
        assert_eq!(ms["availableModels"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn extract_model_state_none_when_absent() {
        let result = serde_json::json!({ "sessionId": "sess-1" });
        assert!(super::extract_model_state(&result).is_none());
    }

    #[test]
    fn resolve_prefers_stable_over_unstable() {
        let result = serde_json::json!({
            "configOptions": [{
                "id": "model",
                "category": "model",
                "options": [
                    { "value": "claude-sonnet-4-20250514", "name": "Sonnet 4" }
                ]
            }],
            "models": {
                "currentModelId": "claude-sonnet-4-20250514",
                "availableModels": [
                    { "modelId": "claude-sonnet-4-20250514", "name": "Sonnet 4" }
                ]
            }
        });
        let method = super::resolve_model_switch_method(&result, "claude-sonnet-4-20250514");
        assert_eq!(
            method,
            Some(super::ModelSwitchMethod::ConfigOption {
                config_id: "model".to_string(),
                option_value: "claude-sonnet-4-20250514".to_string(),
            })
        );
    }

    #[test]
    fn resolve_falls_back_to_unstable() {
        let result = serde_json::json!({
            "models": {
                "currentModelId": "gpt-5",
                "availableModels": [
                    { "modelId": "gpt-5", "name": "GPT-5" },
                    { "modelId": "o3-pro", "name": "o3 Pro" }
                ]
            }
        });
        let method = super::resolve_model_switch_method(&result, "o3-pro");
        assert_eq!(
            method,
            Some(super::ModelSwitchMethod::SetModel {
                model_id: "o3-pro".to_string(),
            })
        );
    }

    #[test]
    fn resolve_returns_none_when_no_match() {
        let result = serde_json::json!({
            "configOptions": [{
                "id": "model",
                "category": "model",
                "options": [{ "value": "claude-sonnet-4-20250514" }]
            }],
            "models": {
                "availableModels": [{ "modelId": "gpt-5" }]
            }
        });
        assert!(super::resolve_model_switch_method(&result, "nonexistent-model").is_none());
    }

    #[test]
    fn resolve_returns_none_when_no_model_info() {
        let result = serde_json::json!({ "sessionId": "sess-1" });
        assert!(super::resolve_model_switch_method(&result, "anything").is_none());
    }

    #[test]
    fn resolve_handles_multiple_config_options() {
        // Agent could have multiple configOptions with category "model"
        // (unlikely but defensive).
        let result = serde_json::json!({
            "configOptions": [
                {
                    "id": "primary-model",
                    "category": "model",
                    "options": [{ "value": "model-a" }]
                },
                {
                    "id": "fallback-model",
                    "category": "model",
                    "options": [{ "value": "model-b" }]
                }
            ]
        });
        let method = super::resolve_model_switch_method(&result, "model-b");
        assert_eq!(
            method,
            Some(super::ModelSwitchMethod::ConfigOption {
                config_id: "fallback-model".to_string(),
                option_value: "model-b".to_string(),
            })
        );
    }

    #[test]
    fn resolve_reads_request_only_config_id_spelling() {
        // Some adapters echo the request-side `configId` back on the response.
        let result = serde_json::json!({
            "configOptions": [{
                "configId": "model",
                "category": "model",
                "options": [{ "value": "haiku" }]
            }]
        });
        assert_eq!(
            super::resolve_model_switch_method(&result, "haiku"),
            Some(super::ModelSwitchMethod::ConfigOption {
                config_id: "model".to_string(),
                option_value: "haiku".to_string(),
            })
        );
    }

    #[test]
    fn resolve_finds_config_options_only_model() {
        // codex-acp shape: the halves disagree — configOptions offers clean ids
        // while availableModels offers reasoning-suffixed ones. Only the stable
        // half can serve `gpt-5.4`.
        let result = serde_json::json!({
            "configOptions": [{
                "id": "model",
                "category": "model",
                "currentValue": "gpt-5.4",
                "options": [{ "value": "gpt-5.4", "name": "GPT-5.4" }]
            }],
            "models": {
                "currentModelId": "gpt-5.3-codex/medium",
                "availableModels": [{ "modelId": "gpt-5.3-codex/medium" }]
            }
        });
        assert_eq!(
            super::resolve_model_switch_method(&result, "gpt-5.4"),
            Some(super::ModelSwitchMethod::ConfigOption {
                config_id: "model".to_string(),
                option_value: "gpt-5.4".to_string(),
            })
        );
    }

    #[test]
    fn config_option_label_prefers_schema_name() {
        let schema = serde_json::json!({ "name": "Haiku", "displayName": "stale" });
        assert_eq!(super::config_option_label(&schema), Some("Haiku"));

        let legacy = serde_json::json!({ "displayName": "Haiku" });
        assert_eq!(super::config_option_label(&legacy), Some("Haiku"));

        assert_eq!(super::config_option_label(&serde_json::json!({})), None);
    }

    // ── model_in_catalog tests ────────────────────────────────────────────

    #[test]
    fn model_in_catalog_true_when_in_config_options() {
        let config_options = vec![serde_json::json!({
            "id": "model",
            "category": "model",
            "options": [
                { "value": "claude-sonnet-4-20250514" },
                { "value": "claude-opus-4-20250514" }
            ]
        })];
        assert!(super::model_in_catalog(
            &config_options,
            None,
            "claude-opus-4-20250514"
        ));
    }

    #[test]
    fn model_in_catalog_true_when_in_available_models() {
        let available = serde_json::json!({
            "currentModelId": "gpt-5",
            "availableModels": [
                { "modelId": "gpt-5" },
                { "modelId": "o3-pro" }
            ]
        });
        assert!(super::model_in_catalog(&[], Some(&available), "o3-pro"));
    }

    #[test]
    fn model_in_catalog_false_when_absent_from_both_halves() {
        let config_options = vec![serde_json::json!({
            "id": "model",
            "options": [{ "value": "claude-sonnet-4-20250514" }]
        })];
        let available = serde_json::json!({
            "availableModels": [{ "modelId": "gpt-5" }]
        });
        assert!(!super::model_in_catalog(
            &config_options,
            Some(&available),
            "nonexistent-model"
        ));
    }

    #[test]
    fn model_in_catalog_false_when_both_halves_empty() {
        assert!(!super::model_in_catalog(&[], None, "anything"));
    }

    // ── Error variant display ─────────────────────────────────────────────

    #[test]
    fn idle_timeout_error_includes_duration() {
        let err = AcpError::IdleTimeout(std::time::Duration::from_secs(320));
        let msg = err.to_string();
        assert!(
            msg.contains("320"),
            "IdleTimeout display should include duration: {msg}"
        );
    }

    #[test]
    fn hard_timeout_error_display() {
        let err = AcpError::HardTimeout {
            silence: std::time::Duration::from_secs(120),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Hard turn timeout"),
            "HardTimeout display: {msg}"
        );
    }

    async fn spawn_script(script: &str) -> AcpClient {
        AcpClient::spawn("bash", &["-c".into(), script.into()], &[], false)
            .await
            .expect("failed to spawn test script")
    }

    #[tokio::test]
    async fn idle_timeout_fires_on_silent_process() {
        let mut client = spawn_script("sleep 10").await;
        let max_dur = std::time::Duration::from_secs(30);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(100),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(
            matches!(result, Err(AcpError::IdleTimeout(_))),
            "expected IdleTimeout, got {result:?}"
        );
    }

    #[tokio::test]
    async fn hard_timeout_fires_when_deadline_is_immediate() {
        let mut client = spawn_script("while true; do echo 'noise'; sleep 0.01; done").await;
        let max_dur = std::time::Duration::from_millis(1);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_secs(60),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(
            matches!(result, Err(AcpError::HardTimeout { .. })),
            "expected HardTimeout, got {result:?}"
        );
    }

    /// `cancel_with_cleanup_grace`'s bounded drain deadline must map to
    /// [`AcpError::CancelDrainTimeout`], never [`AcpError::HardTimeout`] —
    /// the two share an underlying deadline mechanism but must not share
    /// classification, since callers dead-letter a real `HardTimeout` and
    /// must not dead-letter a drain that simply ran past its grace window.
    #[tokio::test]
    async fn cancel_with_cleanup_grace_maps_expiry_to_cancel_drain_timeout() {
        // Agent ignores `session/cancel` on stdin and keeps producing noise
        // forever — never drains within the grace window.
        let mut client = spawn_script("while true; do echo 'noise'; sleep 0.01; done").await;
        client.last_prompt_id = Some(999);
        let grace = std::time::Duration::from_millis(200);
        let result = client
            .cancel_with_cleanup_grace("test-session", grace)
            .await;
        assert!(
            matches!(result, Err(AcpError::CancelDrainTimeout(g)) if g == grace),
            "expected CancelDrainTimeout({grace:?}), got {result:?}"
        );
    }

    #[tokio::test]
    async fn idle_resets_on_stdout_activity() {
        // Send valid JSON (session/update notifications) to reset the idle timer.
        // Non-JSON lines no longer reset idle — only valid JSON notifications do.
        let mut client = spawn_script(
            r#"for i in $(seq 1 10); do echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"text":"thinking"}}}}'; sleep 0.05; done; sleep 10"#,
        )
        .await;
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(200),
                hard_deadline,
                max_dur,
            )
            .await;
        let elapsed = start.elapsed();
        // 10 messages × 50ms = ~500ms of activity, then idle timeout fires after 200ms more
        assert!(elapsed >= std::time::Duration::from_millis(400));
        assert!(elapsed < std::time::Duration::from_secs(3));
        assert!(matches!(result, Err(AcpError::IdleTimeout(_))));
    }

    #[tokio::test]
    async fn response_returned_when_matching_id_arrives() {
        let mut client =
            spawn_script(r#"echo '{"jsonrpc":"2.0","id":42,"result":{"stopReason":"end_turn"}}'"#)
                .await;
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                42,
                std::time::Duration::from_secs(2),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["stopReason"].as_str(), Some("end_turn"));
    }

    #[tokio::test]
    async fn agent_exit_detected_as_eof() {
        let mut client = spawn_script("exit 0").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_secs(2),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(matches!(result, Err(AcpError::AgentExited)));
    }

    /// A message with both `id` and `method` is an agent-initiated request,
    /// not a response. The response matcher must not consume it even if the
    /// id happens to match the expected value.
    #[tokio::test]
    async fn agent_request_with_matching_id_not_consumed_as_response() {
        // The script sends an agent-initiated request (has both id and method)
        // whose id matches what we're waiting for (0), then sends the real
        // response. The request should be dispatched (triggering -32601 since
        // "test/method" is unknown), and the real response should be returned.
        let script = r#"
            echo '{"jsonrpc":"2.0","id":0,"method":"test/method","params":{}}'
            read -t 2 _reply
            echo '{"jsonrpc":"2.0","id":0,"result":{"ok":true}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                0,
                std::time::Duration::from_secs(3),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(result.is_ok(), "expected Ok response, got {result:?}");
        assert_eq!(result.unwrap()["ok"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn idle_fires_before_hard_when_idle_is_shorter() {
        let mut client = spawn_script("sleep 10").await;
        let idle = std::time::Duration::from_millis(100);
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout("test", 999, idle, hard_deadline, max_dur)
            .await;
        assert!(
            matches!(result, Err(AcpError::IdleTimeout(_))),
            "idle should fire before hard when idle << hard, got {result:?}"
        );
    }

    /// Hard-deadline starvation regression (Max's review gate, Eva's required test).
    ///
    /// When the read-loop became a `tokio::select!` with `biased; reader →
    /// steer → sleep_until`, a continuously-ready reader arm could win every
    /// poll and starve the timer arm — silently defeating the hard-deadline
    /// guarantee. The fix is a pre-select deadline check at the top of every
    /// loop iteration; this test pins that behavior.
    ///
    /// Setup: agent emits a **gapless** stream of valid JSON `session/update`
    /// notifications (no `sleep` between lines) so the reader arm is
    /// continuously ready. Each line is valid JSON, so it resets the idle
    /// clock — and we set idle ≫ hard so idle cannot fire first. With
    /// `biased; reader → steer → sleep_until`, the reader arm would win
    /// every poll and `sleep_until` would never be reached. Only the
    /// pre-select deadline check at the top of the loop can stop us.
    ///
    /// Without the pre-select check, this test hangs against the infinite
    /// bash subprocess until the test harness's own outer timeout, and the
    /// returned error would never be `HardTimeout`.
    #[tokio::test]
    async fn hard_deadline_fires_under_continuous_valid_json_stream() {
        // Truly infinite, gapless stream of valid JSON. No `sleep` between
        // echoes — the reader arm is continuously ready, which is the
        // exact starvation scenario the pre-select check guards against.
        // `while :; do echo ...; done` (not a fixed-count `for`) so the
        // subprocess never naturally exits before the hard deadline,
        // regardless of how fast the host drains bash output. Without
        // this, fast hardware drains a bounded loop in < hard_deadline
        // and the reader hits EOF (`AgentExited`) before the timer fires,
        // masking whether the pre-select check actually works.
        let mut client = spawn_script(
            r#"while :; do echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"x"}}}}'; done"#,
        )
        .await;
        let hard = std::time::Duration::from_millis(300);
        let hard_deadline = tokio::time::Instant::now() + hard;
        let idle = std::time::Duration::from_secs(60); // idle ≫ hard
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout("test", 999, idle, hard_deadline, hard)
            .await;
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(AcpError::HardTimeout { .. })),
            "expected HardTimeout under gapless valid-JSON stream, got {result:?} (elapsed {elapsed:?})"
        );
        // Must fire close to the hard deadline, not late. Without the
        // pre-select check the reader arm starves sleep_until and elapsed
        // tracks the bash subprocess lifetime instead.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "HardTimeout fired late ({elapsed:?}); reader arm may be starving sleep_until"
        );
    }

    /// Same as `agent_request_with_matching_id_not_consumed_as_response` but
    /// exercises the non-idle `read_until_response` path (via `send_request`).
    #[tokio::test]
    async fn agent_request_not_consumed_via_send_request() {
        // Script: wait for the initialize request, reply, then send an
        // agent-initiated request with id=1 (matching the next send_request id),
        // wait for the -32601 error reply, then send the real response.
        let script = r#"
            read -t 2 _init
            echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
            read -t 2 _req
            echo '{"jsonrpc":"2.0","id":1,"method":"test/unknown","params":{}}'
            read -t 2 _err_reply
            echo '{"jsonrpc":"2.0","id":1,"result":{"worked":true}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        // initialize consumes id=0
        let _init = client
            .initialize()
            .await
            .expect("initialize should succeed");
        // send_request uses id=1 — the agent's request with id=1 and method
        // must not be consumed as the response.
        let result = client
            .send_request("test/echo", serde_json::json!({}))
            .await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert_eq!(result.unwrap()["worked"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn keepalive_resets_idle_past_deadline() {
        // Keepalive session/update lines every 50ms against a 100ms idle deadline.
        // The turn should survive well past the 100ms deadline (proves the fix).
        let mut client = spawn_script(
            r#"for i in $(seq 1 20); do echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"keepalive"}}}'; sleep 0.05; done; sleep 10"#,
        )
        .await;
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(100),
                hard_deadline,
                max_dur,
            )
            .await;
        let elapsed = start.elapsed();
        // 20 keepalives × 50ms = ~1000ms of activity, then idle fires after 100ms more.
        // Must survive well past the 100ms deadline.
        assert!(
            elapsed >= std::time::Duration::from_millis(500),
            "keepalive should reset idle past the deadline; elapsed only {elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(5));
        assert!(matches!(result, Err(AcpError::IdleTimeout(_))));
    }

    #[tokio::test]
    async fn tool_call_resets_idle_then_silence_times_out() {
        // A tool_call session/update resets the idle timer (belt-and-suspenders path),
        // then silence causes idle timeout. This proves the reset works for tool_call
        // specifically — not just via the general valid-JSON reset at line 839.
        //
        // The script emits a tool_call, waits 80ms (under the 200ms idle), then goes
        // silent. If the tool_call reset didn't fire, idle would fire at 200ms from
        // start. With the reset, idle fires at 80ms + 200ms = ~280ms from start.
        let mut client = spawn_script(
            r#"echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","title":"long_running","kind":"shell"}}}'; sleep 0.08; sleep 10"#,
        )
        .await;
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(200),
                hard_deadline,
                max_dur,
            )
            .await;
        let elapsed = start.elapsed();
        // The tool_call arrives near-instantly and resets idle.
        // Then 80ms of silence, then idle fires at ~280ms from start.
        // Must be > 200ms (proves the reset happened after the tool_call).
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "tool_call should reset idle; elapsed only {elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(2));
        assert!(
            matches!(result, Err(AcpError::IdleTimeout(_))),
            "expected IdleTimeout after silence, got {result:?}"
        );
    }

    #[tokio::test]
    async fn session_new_full_includes_system_prompt_when_some() {
        // Script: respond to initialize, then echo back the session/new request.
        let script = r#"
            read -t 2 _init
            echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
            read -t 2 REQ
            echo '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_test","_receivedRequest":'"$REQ"'}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full("/tmp", vec![], Some("Custom system prompt"))
            .await
            .expect("session_new_full should succeed");

        assert_eq!(resp.session_id, "ses_test");
        let received = &resp.raw["_receivedRequest"];
        assert_eq!(
            received["params"]["systemPrompt"].as_str(),
            Some("Custom system prompt"),
            "systemPrompt should be included in params when Some"
        );
    }

    #[tokio::test]
    async fn session_resume_sends_the_stored_session_id_and_cwd() {
        let script = r#"
            read -t 2 REQ
            echo '{"jsonrpc":"2.0","id":0,"result":{"_receivedRequest":'"$REQ"'}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;

        let resp = client
            .session_resume("/tmp", "ses_stored", vec![])
            .await
            .expect("session_resume should succeed");

        // No sessionId in the response — the requested ID is the fallback.
        assert_eq!(resp.session_id, "ses_stored");
        let received = &resp.raw["_receivedRequest"];
        assert_eq!(received["method"], "session/resume");
        assert_eq!(received["params"]["sessionId"], "ses_stored");
        assert_eq!(received["params"]["cwd"], "/tmp");
        assert!(received["params"]["mcpServers"].is_array());
    }

    #[tokio::test]
    async fn session_resume_preserves_missing_transcript_code() {
        // -32002 is what both claude-agent-acp and codex-acp answer for a
        // session ID with no transcript on disk — the caller distinguishes it
        // from -32601 (no resume support) to decide whether to probe again.
        let script = r#"
            read -t 2 _REQ
            echo '{"jsonrpc":"2.0","id":0,"error":{"code":-32002,"message":"Resource not found: ses_gone"}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        assert!(matches!(
            client.session_resume("/tmp", "ses_gone", vec![]).await,
            Err(AcpError::AgentError { code: -32002, .. })
        ));
    }

    #[tokio::test]
    async fn goose_system_prompt_request_uses_append_contract() {
        let script = r#"
            read -t 2 REQ
            echo '{"jsonrpc":"2.0","id":0,"result":{"_receivedRequest":'"$REQ"'}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        let result = client
            .session_set_goose_system_prompt("ses_goose", "Be terse")
            .await
            .expect("custom request succeeds");
        let received = &result["_receivedRequest"];
        assert_eq!(
            received["method"],
            "_goose/unstable/session/system-prompt/set"
        );
        assert_eq!(received["params"]["sessionId"], "ses_goose");
        assert_eq!(received["params"]["mode"], "append");
        assert_eq!(received["params"]["key"], "buzz");
        assert_eq!(received["params"]["text"], "Be terse");
    }

    #[tokio::test]
    async fn goose_system_prompt_preserves_method_not_found_for_fallback() {
        let script = r#"
            read -t 2 _REQ
            echo '{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        assert!(matches!(
            client
                .session_set_goose_system_prompt("ses_goose", "Be terse")
                .await,
            Err(AcpError::AgentError { code: -32601, .. })
        ));
    }

    #[tokio::test]
    async fn goose_system_prompt_preserves_invalid_params_as_error() {
        let script = r#"
            read -t 2 _REQ
            echo '{"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"Invalid params"}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        assert!(matches!(
            client
                .session_set_goose_system_prompt("ses_goose", "Be terse")
                .await,
            Err(AcpError::AgentError { code: -32602, .. })
        ));
    }

    #[tokio::test]
    async fn session_new_full_omits_system_prompt_when_none() {
        // When system_prompt is None, the field should not appear in params.
        let script = r#"
            read -t 2 _init
            echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
            read -t 2 REQ
            echo '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_test","_receivedRequest":'"$REQ"'}}'
            sleep 1
        "#;
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full("/tmp", vec![], None)
            .await
            .expect("session_new_full should succeed");

        assert_eq!(resp.session_id, "ses_test");
        let received = &resp.raw["_receivedRequest"];
        assert!(
            received["params"]["systemPrompt"].is_null(),
            "systemPrompt should NOT be in params when value is None"
        );
    }

    // ── Goose-native steer scaffold (PR follow-up to #1160) ──────────────

    /// Helper: spawn an inert `cat` subprocess so we have a real AcpClient
    /// to drive `handle_session_update` against. `cat` never writes back,
    /// which is fine — these tests don't read from the agent, they just
    /// feed JSON into the parser.
    async fn spawn_inert_client() -> AcpClient {
        AcpClient::spawn("cat", &[], &[], false)
            .await
            .expect("spawn cat as inert client")
    }

    /// Build a `session/update` JSON-RPC notification carrying a
    /// `session_info_update` with the given `_meta.goose.activeRunId` value.
    /// Pass `None` to omit the `activeRunId` field entirely.
    ///
    /// `_meta` is nested inside the `update` object (per the ACP
    /// `SessionInfoUpdate` schema), matching what goose and buzz-agent
    /// emit on the wire.
    fn session_info_update_msg(active_run_id: Option<serde_json::Value>) -> serde_json::Value {
        let mut goose = serde_json::Map::new();
        if let Some(v) = active_run_id {
            goose.insert("activeRunId".to_string(), v);
        }
        let mut meta = serde_json::Map::new();
        meta.insert("goose".to_string(), serde_json::Value::Object(goose));
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "test-session",
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": serde_json::Value::Object(meta),
                },
            }
        })
    }

    #[tokio::test]
    async fn active_run_id_sets_on_string() {
        let mut client = spawn_inert_client().await;
        assert!(client.active_run_id().is_none(), "starts as None");

        let msg = session_info_update_msg(Some(serde_json::json!("run-abc-123")));
        let _ = client.handle_session_update(&msg);

        assert_eq!(client.active_run_id(), Some("run-abc-123"));
    }

    #[tokio::test]
    async fn active_run_id_clears_on_null() {
        let mut client = spawn_inert_client().await;
        // Set it first
        let set_msg = session_info_update_msg(Some(serde_json::json!("run-xyz")));
        let _ = client.handle_session_update(&set_msg);
        assert_eq!(client.active_run_id(), Some("run-xyz"));

        // Then clear with explicit null
        let clear_msg = session_info_update_msg(Some(serde_json::Value::Null));
        let _ = client.handle_session_update(&clear_msg);
        assert!(
            client.active_run_id().is_none(),
            "explicit null must clear active_run_id"
        );
    }

    #[tokio::test]
    async fn active_run_id_untouched_when_missing() {
        // Field absent entirely — must NOT clear existing state (only an
        // explicit null clears; missing means "no new info this update").
        let mut client = spawn_inert_client().await;
        let set_msg = session_info_update_msg(Some(serde_json::json!("run-stable")));
        let _ = client.handle_session_update(&set_msg);
        assert_eq!(client.active_run_id(), Some("run-stable"));

        // session_info_update with no activeRunId field — leave state alone.
        let missing_msg = session_info_update_msg(None);
        let _ = client.handle_session_update(&missing_msg);
        assert_eq!(
            client.active_run_id(),
            Some("run-stable"),
            "missing activeRunId must leave state untouched"
        );
    }

    #[tokio::test]
    async fn active_run_id_untouched_on_wrong_type() {
        // A number or object in activeRunId is malformed — neither set nor clear.
        let mut client = spawn_inert_client().await;
        let set_msg = session_info_update_msg(Some(serde_json::json!("run-stable")));
        let _ = client.handle_session_update(&set_msg);
        assert_eq!(client.active_run_id(), Some("run-stable"));

        let wrong_type_msg = session_info_update_msg(Some(serde_json::json!(42)));
        let _ = client.handle_session_update(&wrong_type_msg);
        assert_eq!(
            client.active_run_id(),
            Some("run-stable"),
            "non-string/non-null activeRunId must leave state untouched"
        );
    }

    // ── Goose-native steer arm tests ──────────────────────────────────────
    //
    // These exercise the seam between `install_steer_rx` and the read
    // loop's steer arm, isolated from `AgentPool` / `EventQueue` /
    // dispatch. They prove the locked Option-X contract at the read-loop
    // boundary:
    //   1. With `active_run_id == None`, the steer arm acks
    //      `Err(ExpectedRunIdMissing)` and writes nothing — the main
    //      loop's "Err-before-pending" fallback path is reachable.
    //   2. With `active_run_id` set, the steer arm writes the JSON-RPC
    //      request with the matching `expectedRunId` and routes the
    //      response to the ack oneshot as `Success`.
    //
    // We don't test the full mode-gate fork here — that lives in lib.rs
    // and is covered by goose e2e (Eva's lane).

    /// Steer with no `active_run_id` set acks `ExpectedRunIdMissing`
    /// without writing anything. The read loop continues normally and
    /// eventually hits the idle timeout (which is fine — we just need to
    /// observe the ack).
    #[tokio::test]
    async fn native_steer_with_no_active_run_id_acks_expected_run_id_missing() {
        // Quiet process: never emits anything, so the read loop has only
        // the steer arm and the idle timeout to consider.
        let mut client = spawn_script("sleep 10").await;
        assert!(
            client.active_run_id().is_none(),
            "precondition: active_run_id starts as None"
        );

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);

        // Fire-and-forget: send a SteerRequest from a separate task so
        // the read loop picks it up via the select! arm.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::pool::SteerRequest {
                    prompt_blocks: vec!["test steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        // Drive the read loop with short idle timeout so the test
        // doesn't hang. The expected_id is intentionally never going to
        // be matched (the script writes nothing); the read loop will
        // exit via IdleTimeout shortly after the steer arm fires.
        let idle = std::time::Duration::from_millis(500);
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let read_result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        // Read loop exit shape: IdleTimeout (no agent activity).
        assert!(
            matches!(read_result, Err(AcpError::IdleTimeout(_))),
            "expected IdleTimeout once steer was acked + script stayed silent, got {read_result:?}"
        );

        // Ack must be ExpectedRunIdMissing — the steer arm bailed out
        // without writing because active_run_id was None at write time.
        let ack = ack_rx
            .await
            .expect("ack oneshot must have received a SteerAck");
        match ack {
            crate::pool::SteerAck::Err(crate::pool::SteerError::ExpectedRunIdMissing) => {}
            other => panic!("expected SteerAck::Err(ExpectedRunIdMissing), got {other:?}"),
        }
    }

    /// Steer with `active_run_id` set writes the JSON-RPC request and
    /// routes the matching response to the ack oneshot as `Success`.
    /// Verifies the wire shape (`sessionId` + `expectedRunId` + `prompt`)
    /// indirectly: the bash script emits a response keyed by the steer
    /// id (0), and `Success` only fires if the read loop matched that
    /// id to its `pending_steer` entry.
    #[tokio::test]
    async fn native_steer_with_active_run_id_routes_response_to_ack() {
        // Script: pause briefly so the test task can install the steer
        // and we can be sure the response doesn't race ahead of the
        // write — then emit the steer response (id=0 because next_id
        // starts at 0 and the steer is the first request the read loop
        // writes), then idle. This is a JSON-RPC success response with
        // a `stopReason` payload (matching the shape goose uses for
        // steer responses in fake_llm.rs).
        let script = "sleep 0.5; \
                      echo '{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"stopReason\":\"end_turn\"}}'; \
                      sleep 10";
        let mut client = spawn_script(script).await;

        // Set active_run_id via a synthesized session_info_update so the
        // steer arm has a non-None value to read at write time.
        let update = session_info_update_msg(Some(serde_json::json!("run-42")));
        let _ = client.handle_session_update(&update);
        assert_eq!(client.active_run_id(), Some("run-42"));

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::pool::SteerRequest {
                    prompt_blocks: vec!["test steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        // Drive the read loop. Expected_id 999 will never be emitted by
        // the script so the read loop exits via idle timeout after the
        // steer response is routed to ack.
        let idle = std::time::Duration::from_secs(2);
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let read_result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        // Read loop exit: IdleTimeout (no further activity after the
        // routed steer response). AgentExited would also be a valid
        // exit if the bash script terminated early; either is fine —
        // what matters is the ack.
        assert!(
            matches!(
                read_result,
                Err(AcpError::IdleTimeout(_)) | Err(AcpError::AgentExited)
            ),
            "expected IdleTimeout or AgentExited after steer ack, got {read_result:?}"
        );

        // Ack must be Success: the steer response (id=0) was routed to
        // pending_steer.ack_tx.
        let ack = ack_rx
            .await
            .expect("ack oneshot must have received a SteerAck");
        match ack {
            crate::pool::SteerAck::Success => {}
            other => panic!("expected SteerAck::Success, got {other:?}"),
        }
    }

    /// Steer-success renewal keeps the turn alive past the original hard
    /// deadline. This is the red-on-old/green-on-new test for the core bug
    /// fix (acp.rs:1440-1444): without renewal, the read loop returns
    /// `HardTimeout` before the prompt response arrives.
    ///
    /// Timeline:
    ///   t≈0:    read loop starts, `hard_deadline = now + 1s`
    ///   t≈0.5s: script emits steer response (id=0) → Success renewal
    ///           moves `hard_deadline` to `now + 3s` (≈3.5s from start)
    ///   t≈1.5s: script emits prompt response (id=999) → `Ok`
    ///
    /// Old code: `HardTimeout` at t≈1s (before prompt response).
    /// New code: deadline renewed at t≈0.5s → prompt response at t≈1.5s → `Ok`.
    #[tokio::test]
    async fn steer_success_renews_hard_deadline_and_survives_past_original() {
        let script = "sleep 0.5; \
                      echo '{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"stopReason\":\"end_turn\"}}'; \
                      sleep 1; \
                      echo '{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{\"done\":true}}'";
        let mut client = spawn_script(script).await;

        let update = session_info_update_msg(Some(serde_json::json!("run-99")));
        let _ = client.handle_session_update(&update);

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::pool::SteerRequest {
                    prompt_blocks: vec!["steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        let idle = std::time::Duration::from_secs(10);
        let max_dur = std::time::Duration::from_secs(3);
        let hard_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        assert!(
            result.is_ok(),
            "expected Ok (prompt response after renewed deadline), got {result:?}"
        );
        assert_eq!(result.unwrap()["done"], serde_json::json!(true));

        let ack = ack_rx
            .await
            .expect("ack oneshot must have received a SteerAck");
        match ack {
            crate::pool::SteerAck::Success => {}
            other => panic!("expected SteerAck::Success, got {other:?}"),
        }
    }

    // ── Elicitation ───────────────────────────────────────────────────────

    /// An `elicitation/create` params object in the shape adapters send for a
    /// single-select AskUserQuestion: the question on `message`, a short chip
    /// label on the field's `title`, a titled `oneOf` enum, and the
    /// per-question free-text sibling.
    fn ask_params() -> serde_json::Value {
        serde_json::json!({
            "mode": "form",
            "sessionId": "sess-test",
            "message": "Which database?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "question_0": {
                        "type": "string",
                        // The chip label the real adapter sends alongside the
                        // question — never the question itself.
                        "title": "Database",
                        "oneOf": [
                            {"const": "Postgres", "title": "Postgres", "description": "mature"},
                            {"const": "SQLite", "title": "SQLite"},
                        ],
                    },
                    "question_0_custom": {"type": "string", "title": "Other"},
                },
            },
        })
    }

    /// A stub relay that answers every submission with `status`, so tests can
    /// drive both the published-question and the failed-publish paths.
    /// `4xx` is non-retriable, so a failing submit fails fast.
    async fn stub_relay(status: &'static str) -> (crate::relay::RestClient, TestServer) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub relay");
        let base_url = format!("http://{}", listener.local_addr().expect("stub relay addr"));
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0; 8192];
                let _ = socket.read(&mut request).await;
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let rest = crate::relay::RestClient {
            http: reqwest::Client::new(),
            base_url,
            keys: nostr::Keys::generate(),
            auth_tag_json: None,
        };
        (rest, TestServer(server))
    }

    /// Aborts the stub relay when the test drops it.
    struct TestServer(tokio::task::JoinHandle<()>);

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// An ask surface whose questions reach the relay, plus the state the main
    /// loop reads to decide a message is an answer, and the stub relay's guard.
    async fn test_ask() -> (
        crate::pool::ElicitationAsk,
        crate::pool::ElicitationState,
        TestServer,
    ) {
        test_ask_with_relay("200 OK").await
    }

    async fn test_ask_with_relay(
        status: &'static str,
    ) -> (
        crate::pool::ElicitationAsk,
        crate::pool::ElicitationState,
        TestServer,
    ) {
        let (rest, server) = stub_relay(status).await;
        let state = crate::pool::ElicitationState::default();
        let ask = crate::pool::ElicitationAsk::new(
            rest,
            uuid::Uuid::nil(),
            crate::queue::ThreadTags::default(),
            None,
            state.clone(),
        );
        (ask, state, server)
    }

    /// Send `reply` the way the main loop does: only once the read loop has
    /// published a question and armed the shared state.
    async fn reply_when_asked(
        state: crate::pool::ElicitationState,
        tx: tokio::sync::mpsc::Sender<crate::pool::ElicitationReply>,
        reply: crate::pool::ElicitationReply,
    ) {
        while state.question_event_id().is_none() {
            tokio::task::yield_now().await;
        }
        tx.send(reply).await.expect("reply send should succeed");
    }

    fn ask_fields() -> Vec<ElicitationField> {
        parse_elicitation_fields(&ask_params()).expect("form must parse")
    }

    fn answer(fields: &[ElicitationField], reply: &str) -> serde_json::Value {
        let mut answers = serde_json::Map::new();
        answer_elicitation_field(&fields[0], reply, &mut answers);
        serde_json::Value::Object(answers)
    }

    #[test]
    fn elicitation_form_parses_questions_and_pairs_custom_fields() {
        let fields = ask_fields();
        assert_eq!(
            fields.len(),
            1,
            "`question_0_custom` is not its own question"
        );
        assert_eq!(fields[0].name, "question_0");
        assert_eq!(fields[0].custom.as_deref(), Some("question_0_custom"));
        // A single-question form carries the prompt on `message`, not the field.
        assert_eq!(fields[0].prompt, "Which database?");
        assert_eq!(fields[0].options.len(), 2);
        assert_eq!(fields[0].options[0].description.as_deref(), Some("mature"));
    }

    #[test]
    fn elicitation_non_form_mode_is_not_answerable() {
        let params = serde_json::json!({
            "mode": "url",
            "sessionId": "sess-test",
            "message": "Log in",
            "url": "https://example.test/login",
        });
        assert!(parse_elicitation_fields(&params).is_none());
    }

    #[test]
    fn elicitation_reply_selects_by_index_or_label() {
        let fields = ask_fields();
        assert_eq!(
            answer(&fields, "2"),
            serde_json::json!({"question_0": "SQLite"})
        );
        assert_eq!(
            answer(&fields, "  postgres "),
            serde_json::json!({"question_0": "Postgres"}),
            "labels match case-insensitively and ignore padding"
        );
        assert_eq!(
            answer(&fields, "3"),
            serde_json::json!({"question_0_custom": "3"}),
            "an out-of-range index is free text, not a panic"
        );
    }

    #[test]
    fn elicitation_reply_naming_no_option_goes_to_the_custom_field() {
        assert_eq!(
            answer(&ask_fields(), "DuckDB"),
            serde_json::json!({"question_0_custom": "DuckDB"})
        );
    }

    #[test]
    fn elicitation_reply_without_a_custom_field_answers_the_field_itself() {
        // A bare MCP-shaped form: no options, no free-text sibling, and a
        // non-string type to coerce. Guards the non-AskUserQuestion paths that
        // the same capability lights up.
        let params = serde_json::json!({
            "mode": "form",
            "sessionId": "sess-test",
            "message": "Proceed?",
            "requestedSchema": {
                "type": "object",
                "properties": {"confirm": {"type": "boolean"}},
            },
        });
        let fields = parse_elicitation_fields(&params).expect("form must parse");
        assert!(fields[0].custom.is_none());
        assert_eq!(answer(&fields, "yes"), serde_json::json!({"confirm": true}));
        assert_eq!(
            answer(&fields, "maybe"),
            serde_json::json!({"confirm": "maybe"}),
            "an uncoercible reply is kept verbatim rather than dropped"
        );
    }

    #[test]
    fn elicitation_multi_select_reply_is_a_list_of_option_values() {
        let params = serde_json::json!({
            "mode": "form",
            "sessionId": "sess-test",
            "message": "Which languages?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "question_0": {
                        "type": "array",
                        "items": {"anyOf": [
                            {"const": "Rust", "title": "Rust"},
                            {"const": "Go", "title": "Go"},
                            {"const": "Zig", "title": "Zig"},
                        ]},
                    },
                },
            },
        });
        let fields = parse_elicitation_fields(&params).expect("form must parse");
        assert_eq!(
            answer(&fields, "1, zig"),
            serde_json::json!({"question_0": ["Rust", "Zig"]})
        );
    }

    #[test]
    fn elicitation_question_renders_an_actionable_fallback() {
        let fields = ask_fields();
        let body = render_elicitation_field(&fields[0], 0, 1);
        assert!(
            !body.contains("<!--"),
            "no HTML comment may ride the body — a marker-blind client renders it literally: {body}"
        );
        assert!(body.contains("**Which database?**"));
        assert!(body.contains("1. Postgres — mature"));
        assert!(body.contains("2. SQLite"));
        assert!(
            body.contains("Reply with the number"),
            "a marker-blind client must still show how to answer: {body}"
        );
        assert!(
            !body.contains("Question"),
            "a single-question form carries no numbering header: {body}"
        );
        let second_of_three = render_elicitation_field(&fields[0], 1, 3);
        assert!(
            second_of_three.contains("Question 2 of 3"),
            "multi-question forms number the question the owner is on: {second_of_three}"
        );
    }

    #[test]
    fn elicitation_ask_tag_carries_the_card_structure() {
        let fields = ask_fields();
        let tag = elicitation_ask_tag(&fields[0], 1, 3).expect("a small form fits the tag");
        assert_eq!(tag[0], "ask");
        let payload: serde_json::Value =
            serde_json::from_str(&tag[1]).expect("the tag value is JSON");
        assert_eq!(payload["v"], 1);
        assert_eq!(payload["question"], "Which database?");
        // Labels, not wire values: a card echoes the label back and
        // `ElicitationField::select` resolves it case-insensitively, so the
        // answer path needs no harness change.
        assert_eq!(payload["options"][0]["label"], "Postgres");
        assert_eq!(payload["options"][0]["description"], "mature");
        assert_eq!(payload["options"][1]["label"], "SQLite");
        assert!(payload["options"][1].get("description").is_none());
        assert_eq!(payload["multiSelect"], false);
        assert_eq!(
            payload["allowFreeText"], true,
            "a paired `_custom` sibling accepts an answer naming no option"
        );
        assert_eq!(payload["index"], 1);
        assert_eq!(payload["total"], 3);
        assert!(
            fields[0]
                .select(payload["options"][0]["label"].as_str().unwrap())
                .is_some(),
            "every label the card offers must resolve back to an option"
        );
    }

    /// Build the `ask` tag payload for the sole field of a one-field form.
    fn ask_payload(properties: serde_json::Value) -> serde_json::Value {
        let params = serde_json::json!({
            "mode": "form",
            "sessionId": "sess-test",
            "message": "Which database?",
            "requestedSchema": {"type": "object", "properties": properties},
        });
        let fields = parse_elicitation_fields(&params).expect("form must parse");
        let tag = elicitation_ask_tag(&fields[0], 0, 1).expect("a small form fits the tag");
        serde_json::from_str(&tag[1]).expect("the tag value is JSON")
    }

    #[test]
    fn elicitation_bare_single_select_advertises_free_text() {
        // The common codex/claude shape: an enum with no `_custom` sibling.
        // The answer path has always accepted words that name no option, so
        // the card must offer the box rather than forcing the owner out of it.
        let payload = ask_payload(serde_json::json!({
            "question_0": {"type": "string", "enum": ["Postgres", "SQLite"]},
        }));
        assert_eq!(payload["multiSelect"], false);
        assert_eq!(
            payload["allowFreeText"], true,
            "a bare single-select accepts free text: `select` misses and the \
             reply falls through to `coerce_elicitation_answer`"
        );

        // Same for the titled `oneOf` spelling of a bare single-select.
        let titled = ask_payload(serde_json::json!({
            "question_0": {
                "type": "string",
                "oneOf": [
                    {"const": "Postgres", "title": "Postgres"},
                    {"const": "SQLite", "title": "SQLite"},
                ],
            },
        }));
        assert_eq!(titled["allowFreeText"], true);
    }

    #[test]
    fn elicitation_free_text_answer_to_a_bare_single_select_reaches_the_agent() {
        // The round trip the synthesized flag promises: the owner types words
        // naming no option, and the agent receives them under the field's own
        // key rather than losing them.
        let params = serde_json::json!({
            "mode": "form",
            "sessionId": "sess-test",
            "message": "Which database?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "question_0": {"type": "string", "enum": ["Postgres", "SQLite"]},
                },
            },
        });
        let fields = parse_elicitation_fields(&params).expect("form must parse");
        assert!(fields[0].custom.is_none(), "no free-text sibling exists");
        assert_eq!(
            answer(&fields, "DuckDB, but only if it embeds"),
            serde_json::json!({"question_0": "DuckDB, but only if it embeds"}),
            "free text lands verbatim under the field's own key"
        );
        // The option path is untouched by the wider flag.
        assert_eq!(
            answer(&fields, "2"),
            serde_json::json!({"question_0": "SQLite"})
        );
    }

    #[test]
    fn elicitation_paired_custom_sibling_keeps_its_existing_behavior() {
        let payload = ask_payload(serde_json::json!({
            "question_0": {"type": "string", "enum": ["Postgres", "SQLite"]},
            "question_0_custom": {"type": "string", "title": "Other"},
        }));
        assert_eq!(payload["allowFreeText"], true);
        assert_eq!(
            answer(&ask_fields(), "DuckDB"),
            serde_json::json!({"question_0_custom": "DuckDB"}),
            "a paired sibling still receives the free-text answer"
        );
    }

    #[test]
    fn elicitation_multi_select_does_not_synthesize_free_text() {
        // A free-text reply to an array field is comma-split into arbitrary
        // strings where the agent asked for enum members — worse than the
        // numbered body already collects, so multi-select stays opt-in.
        let payload = ask_payload(serde_json::json!({
            "question_0": {
                "type": "array",
                "items": {"enum": ["Rust", "Go", "Zig"]},
            },
        }));
        assert_eq!(payload["multiSelect"], true);
        assert_eq!(payload["allowFreeText"], false);

        // …unless the adapter pairs one explicitly.
        let paired = ask_payload(serde_json::json!({
            "question_0": {
                "type": "array",
                "items": {"enum": ["Rust", "Go", "Zig"]},
            },
            "question_0_custom": {"type": "string", "title": "Other"},
        }));
        assert_eq!(paired["multiSelect"], true);
        assert_eq!(paired["allowFreeText"], true);
    }

    #[test]
    fn elicitation_optionless_fields_still_advertise_free_text() {
        // Unchanged shapes: a field with no options is free-text only whatever
        // its type says.
        for ty in ["string", "boolean", "integer", "number"] {
            let payload = ask_payload(serde_json::json!({
                "question_0": {"type": ty},
            }));
            assert_eq!(
                payload["allowFreeText"], true,
                "an optionless {ty} field is answerable only as free text"
            );
        }
    }

    #[test]
    fn elicitation_ask_tag_is_dropped_when_it_would_bloat_the_event() {
        let params = serde_json::json!({
            "mode": "form",
            "sessionId": "sess-test",
            "message": "x".repeat(ASK_TAG_MAX_BYTES + 1),
            "requestedSchema": {
                "type": "object",
                "properties": {"question_0": {"type": "string"}},
            },
        });
        let fields = parse_elicitation_fields(&params).expect("form must parse");
        assert!(
            elicitation_ask_tag(&fields[0], 0, 1).is_none(),
            "an oversized question publishes body-only rather than a huge tag"
        );
    }

    /// A `session/prompt` turn in which the agent asks one question, waits for
    /// the answer, and only then completes. Proves the whole loop: publish,
    /// park, route the owner's reply, and write the accept response.
    #[tokio::test]
    async fn elicitation_round_trip_answers_the_agent_and_completes_the_turn() {
        // The agent blocks on its own stdin: the prompt response is only
        // emitted once the elicitation response line arrives, so a hang here
        // means the harness never answered.
        let script = "echo '{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"elicitation/create\",\
                      \"params\":{\"mode\":\"form\",\"sessionId\":\"s\",\"message\":\"Which database?\",\
                      \"requestedSchema\":{\"type\":\"object\",\"properties\":{\
                      \"question_0\":{\"type\":\"string\",\"oneOf\":[{\"const\":\"Postgres\",\"title\":\"Postgres\"},\
                      {\"const\":\"SQLite\",\"title\":\"SQLite\"}]},\
                      \"question_0_custom\":{\"type\":\"string\",\"title\":\"Other\"}}}}}'; \
                      read -r response; \
                      echo \"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":999,\\\"result\\\":{\\\"answered\\\":$response}}\"";
        let mut client = spawn_script(script).await;
        let (reply_tx, reply_rx) = tokio::sync::mpsc::channel(1);
        let (ask, state, _relay) = test_ask().await;
        client.install_elicitation(ask, reply_rx);

        let reply_task = tokio::spawn(reply_when_asked(
            state,
            reply_tx,
            crate::pool::ElicitationReply::Answer("2".into()),
        ));

        let idle = std::time::Duration::from_secs(5);
        let result = client
            .read_until_response_with_idle_timeout(
                "s",
                999,
                idle,
                tokio::time::Instant::now() + idle,
                idle,
            )
            .await
            .expect("turn should complete once the question is answered");
        reply_task.await.expect("reply task should complete");

        assert_eq!(
            result["answered"],
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "result": {"action": "accept", "content": {"question_0": "SQLite"}}
            })
        );
    }

    /// `!skip` declines, which the agent reads as "the user skipped" and the
    /// turn continues — distinct from the cancel that teardown writes.
    #[tokio::test]
    async fn elicitation_skip_declines() {
        let script = "echo '{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"elicitation/create\",\
                      \"params\":{\"mode\":\"form\",\"sessionId\":\"s\",\"message\":\"Which database?\",\
                      \"requestedSchema\":{\"type\":\"object\",\"properties\":{\
                      \"question_0\":{\"type\":\"string\",\"enum\":[\"Postgres\"]}}}}}'; \
                      read -r response; \
                      echo \"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":999,\\\"result\\\":{\\\"answered\\\":$response}}\"";
        let mut client = spawn_script(script).await;
        let (reply_tx, reply_rx) = tokio::sync::mpsc::channel(1);
        let (ask, state, _relay) = test_ask().await;
        client.install_elicitation(ask, reply_rx);

        let reply_task = tokio::spawn(reply_when_asked(
            state,
            reply_tx,
            crate::pool::ElicitationReply::Skip,
        ));

        let idle = std::time::Duration::from_secs(5);
        let result = client
            .read_until_response_with_idle_timeout(
                "s",
                999,
                idle,
                tokio::time::Instant::now() + idle,
                idle,
            )
            .await
            .expect("turn should complete once the question is skipped");
        reply_task.await.expect("reply task should complete");

        assert_eq!(
            result["answered"]["result"],
            serde_json::json!({"action": "decline"})
        );
    }

    /// A request with nowhere to render — no ask surface installed, as on the
    /// `initialize` / `session/new` path — is cancelled rather than parked, so
    /// the agent is never left waiting on a question nobody will see.
    #[tokio::test]
    async fn elicitation_without_an_ask_surface_is_cancelled() {
        let script = "echo '{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"elicitation/create\",\
                      \"params\":{\"mode\":\"form\",\"sessionId\":\"s\",\"message\":\"Which database?\",\
                      \"requestedSchema\":{\"type\":\"object\",\"properties\":{\
                      \"question_0\":{\"type\":\"string\",\"enum\":[\"Postgres\"]}}}}}'; \
                      read -r response; \
                      echo \"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":999,\\\"result\\\":{\\\"answered\\\":$response}}\"";
        let mut client = spawn_script(script).await;

        let idle = std::time::Duration::from_secs(5);
        let result = client
            .read_until_response_with_idle_timeout(
                "s",
                999,
                idle,
                tokio::time::Instant::now() + idle,
                idle,
            )
            .await
            .expect("turn should complete without a human in the loop");

        assert_eq!(
            result["answered"]["result"],
            serde_json::json!({"action": "cancel"})
        );
        assert!(client.pending_elicitation.is_none());
    }

    /// A question the relay rejected is a question nobody can answer: it is
    /// cancelled within seconds rather than parking the turn until its hard
    /// cap and surfacing as a generic "exceeded the maximum duration".
    #[tokio::test]
    async fn elicitation_the_relay_rejects_is_cancelled_not_parked() {
        let script = "echo '{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"elicitation/create\",\
                      \"params\":{\"mode\":\"form\",\"sessionId\":\"s\",\"message\":\"Which database?\",\
                      \"requestedSchema\":{\"type\":\"object\",\"properties\":{\
                      \"question_0\":{\"type\":\"string\",\"enum\":[\"Postgres\"]}}}}}'; \
                      read -r response; \
                      echo \"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":999,\\\"result\\\":{\\\"answered\\\":$response}}\"";
        let mut client = spawn_script(script).await;
        let (_reply_tx, reply_rx) = tokio::sync::mpsc::channel(1);
        let (ask, state, _relay) = test_ask_with_relay("400 Bad Request").await;
        client.install_elicitation(ask, reply_rx);

        let idle = std::time::Duration::from_secs(5);
        let result = client
            .read_until_response_with_idle_timeout(
                "s",
                999,
                idle,
                tokio::time::Instant::now() + idle,
                idle,
            )
            .await
            .expect("the turn should complete rather than park on an unseen question");

        assert_eq!(
            result["answered"]["result"],
            serde_json::json!({"action": "cancel"})
        );
        assert!(client.pending_elicitation.is_none());
        assert_eq!(
            state.question_event_id(),
            None,
            "an unpublished question must not arm the main loop's reply routing"
        );
    }

    /// The silent-agent guard is suspended while a question is outstanding —
    /// a human may think for longer than `idle_timeout` — but the hard
    /// deadline keeps bounding the turn.
    #[tokio::test]
    async fn parked_elicitation_suspends_the_idle_clock_but_not_the_hard_deadline() {
        let script = "echo '{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"elicitation/create\",\
                      \"params\":{\"mode\":\"form\",\"sessionId\":\"s\",\"message\":\"Which database?\",\
                      \"requestedSchema\":{\"type\":\"object\",\"properties\":{\
                      \"question_0\":{\"type\":\"string\",\"enum\":[\"Postgres\"]}}}}}'; \
                      sleep 30";
        let mut client = spawn_script(script).await;
        let (_reply_tx, reply_rx) = tokio::sync::mpsc::channel(1);
        let (ask, _state, _relay) = test_ask().await;
        client.install_elicitation(ask, reply_rx);

        let idle = std::time::Duration::from_millis(200);
        let max_duration = std::time::Duration::from_secs(2);
        let result = client
            .read_until_response_with_idle_timeout(
                "s",
                999,
                idle,
                tokio::time::Instant::now() + max_duration,
                max_duration,
            )
            .await;

        assert!(
            matches!(result, Err(AcpError::HardTimeout { .. })),
            "expected the hard deadline to fire, not the suspended idle clock, got {result:?}"
        );
    }

    #[tokio::test]
    async fn cancel_request_drops_the_parked_elicitation_without_responding() {
        let mut client = spawn_script("sleep 10").await;
        let (_reply_tx, reply_rx) = tokio::sync::mpsc::channel(1);
        let (ask, _state, _relay) = test_ask().await;
        client.install_elicitation(ask, reply_rx);
        client
            .handle_elicitation_request(&serde_json::json!({"id": 7, "params": ask_params()}), true)
            .await
            .expect("the question should park");
        assert!(client.pending_elicitation.is_some());

        client.handle_cancel_request(&serde_json::json!({"params": {"requestId": 8}}));
        assert!(
            client.pending_elicitation.is_some(),
            "a cancel naming another request must not drop this one"
        );
        client.handle_cancel_request(&serde_json::json!({"params": {"requestId": 7}}));
        assert!(client.pending_elicitation.is_none());
    }

    /// A reply the main loop sent for a question the agent then cancelled sits
    /// unread in the channel. The next question must discard it rather than
    /// answer itself with the previous question's text.
    #[tokio::test]
    async fn stale_reply_does_not_answer_the_next_question() {
        let script = "echo '{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"elicitation/create\",\
                      \"params\":{\"mode\":\"form\",\"sessionId\":\"s\",\"message\":\"Which database?\",\
                      \"requestedSchema\":{\"type\":\"object\",\"properties\":{\
                      \"question_0\":{\"type\":\"string\",\"enum\":[\"Postgres\"]}}}}}'; \
                      sleep 30";
        let mut client = spawn_script(script).await;
        let (reply_tx, reply_rx) = tokio::sync::mpsc::channel(1);
        let (ask, _state, _relay) = test_ask().await;
        client.install_elicitation(ask, reply_rx);
        reply_tx
            .send(crate::pool::ElicitationReply::Answer("Postgres".into()))
            .await
            .expect("the stale reply should queue");

        let idle = std::time::Duration::from_millis(200);
        let max_duration = std::time::Duration::from_secs(2);
        let result = client
            .read_until_response_with_idle_timeout(
                "s",
                999,
                idle,
                tokio::time::Instant::now() + max_duration,
                max_duration,
            )
            .await;

        assert!(
            matches!(result, Err(AcpError::HardTimeout { .. })),
            "the stale reply must be dropped, leaving the question parked, got {result:?}"
        );
    }

    // ── Goose usage notification integration ──────────────────────────────

    /// Build a `_goose/unstable/session/update` JSON-RPC notification.
    fn goose_usage_update_msg(
        session_id: &str,
        input: u64,
        output: u64,
        cost: Option<f64>,
    ) -> serde_json::Value {
        let mut update = serde_json::json!({
            "sessionUpdate": "usage_update",
            "used": input + output,
            "contextLimit": 200000u64,
            "accumulatedInputTokens": input,
            "accumulatedOutputTokens": output,
        });
        if let Some(c) = cost {
            update["accumulatedCost"] = serde_json::json!(c);
        }
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "_goose/unstable/session/update",
            "params": {
                "sessionId": session_id,
                "update": update
            }
        })
    }

    #[tokio::test]
    async fn goose_usage_notification_recorded_and_take_returns_usage() {
        let mut client = spawn_inert_client().await;
        assert!(client.take_turn_usage().is_none(), "starts empty");

        // begin_turn before sending the prompt — mirrors the real call flow.
        client.goose_usage.begin_turn("s1");
        let msg = goose_usage_update_msg("s1", 1000, 200, Some(0.01));
        client.handle_goose_usage_update(&msg);

        let usage = client
            .take_turn_usage()
            .expect("usage should be present after notification");
        assert_eq!(usage.session_id, "s1");
        assert_eq!(usage.turn_seq, 1);
        assert!(!usage.delta_reliable, "first turn must be unreliable");
        assert_eq!(usage.cumulative_input_tokens, 1000);
        assert_eq!(usage.cumulative_output_tokens, 200);
        assert_eq!(usage.cumulative_cost_usd, Some(0.01));

        // Second take must be None.
        assert!(
            client.take_turn_usage().is_none(),
            "take after drain is None"
        );
    }

    #[tokio::test]
    async fn goose_usage_second_turn_delta_reliable() {
        let mut client = spawn_inert_client().await;
        // Turn 1.
        client.goose_usage.begin_turn("s2");
        client.handle_goose_usage_update(&goose_usage_update_msg("s2", 1000, 200, None));
        let _ = client.take_turn_usage();
        // Turn 2.
        client.goose_usage.begin_turn("s2");
        client.handle_goose_usage_update(&goose_usage_update_msg("s2", 1800, 450, None));
        let usage = client.take_turn_usage().expect("turn 2 usage");
        assert!(usage.delta_reliable);
        assert_eq!(usage.turn_input_tokens, Some(800));
        assert_eq!(usage.turn_output_tokens, Some(250));
    }

    #[tokio::test]
    async fn goose_usage_malformed_notification_does_not_panic() {
        let mut client = spawn_inert_client().await;
        // Missing params entirely.
        let bad = serde_json::json!({"jsonrpc":"2.0","method":"_goose/unstable/session/update"});
        client.handle_goose_usage_update(&bad);
        assert!(client.take_turn_usage().is_none());

        // params present but wrong shape.
        let bad2 = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "_goose/unstable/session/update",
            "params": { "oops": true }
        });
        client.handle_goose_usage_update(&bad2);
        assert!(client.take_turn_usage().is_none());
    }

    #[test]
    fn agent_error_from_json_falls_back_to_full_json_when_message_missing() {
        // Errors without a string `message` field (e.g. only a `data` field) must
        // not be silently truncated to "unknown error" — the full JSON is preserved.
        let error = serde_json::json!({"code": -32000, "data": "quota exceeded"});
        match super::agent_error_from_json(&error) {
            AcpError::AgentError { code, message } => {
                assert_eq!(code, -32000);
                assert!(
                    message.contains("quota exceeded"),
                    "expected full JSON in message, got: {message}"
                );
            }
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    #[test]
    fn agent_error_from_json_uses_message_field_when_present() {
        let error = serde_json::json!({"code": -32001, "message": "auth denied"});
        match super::agent_error_from_json(&error) {
            AcpError::AgentError { code, message } => {
                assert_eq!(code, -32001);
                assert_eq!(message, "auth denied");
            }
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    // ── build_codex_config_env ────────────────────────────────────────────────

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const GENERATED: &str = r#"{"sandbox_workspace_write":{"network_access":true}}"#;

    #[test]
    fn build_codex_config_env_returns_none_when_no_codex_config_in_extra_env() {
        // Non-Codex agents: extra_env has no CODEX_CONFIG → None regardless of signal.
        let extra = env(&[("GOOSE_PROVIDER", "openai")]);
        let result = build_codex_config_env(&extra, None, false).unwrap();
        assert_eq!(
            result, None,
            "no CODEX_CONFIG in extra_env must return None"
        );
    }

    #[test]
    fn build_codex_config_env_generated_only_single_entry_with_signal_true_merges_with_parent() {
        // No persona: Buzz injects one CODEX_CONFIG; signal=true.
        // Parent may have its own CODEX_CONFIG — deep_merge applies, network_access forced.
        let extra = env(&[("CODEX_CONFIG", GENERATED)]);
        let parent =
            r#"{"some_operator_key":"val","sandbox_workspace_write":{"operator_key":"keep"}}"#;
        let merged = build_codex_config_env(&extra, Some(parent), true)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        // network_access forced true even though only one entry in extra_env.
        assert_eq!(
            v["sandbox_workspace_write"]["network_access"], true,
            "network_access must be forced true with signal=true"
        );
        // Operator key preserved via deep_merge.
        assert_eq!(
            v["sandbox_workspace_write"]["operator_key"], "keep",
            "operator nested key must survive"
        );
        assert_eq!(
            v["some_operator_key"], "val",
            "operator top-level key must survive"
        );
    }

    #[test]
    fn build_codex_config_env_persona_only_signal_false_returns_none() {
        // Persona set CODEX_CONFIG; Buzz did not inject a generated overlay (signal=false).
        // Must return None — no merging, no sandbox widening.
        let persona = r#"{"some_feature":"on"}"#;
        let extra = env(&[("CODEX_CONFIG", persona)]);
        let result = build_codex_config_env(&extra, None, false).unwrap();
        assert_eq!(
            result, None,
            "persona-only CODEX_CONFIG with signal=false must return None"
        );
    }

    #[test]
    fn build_codex_config_env_returns_none_for_persona_only_no_generated_overlay() {
        // Alias: same scenario as above, confirms the old count-based path no longer exists.
        let persona = r#"{"some_feature":"on"}"#;
        let extra = env(&[("CODEX_CONFIG", persona)]);
        let result = build_codex_config_env(&extra, None, false).unwrap();
        assert_eq!(
            result, None,
            "persona-only CODEX_CONFIG with signal=false must return None"
        );
    }

    #[test]
    fn build_codex_config_env_sets_network_access_from_scratch() {
        // Persona + generated overlay, signal=true: network_access is forced true.
        let persona = r#"{}"#;
        let extra = env(&[("CODEX_CONFIG", persona), ("CODEX_CONFIG", GENERATED)]);
        let merged = build_codex_config_env(&extra, None, true).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["sandbox_workspace_write"]["network_access"], true);
    }

    #[test]
    fn build_codex_config_env_persona_keys_survive_merge() {
        // Persona has CODEX_CONFIG with unrelated keys; generated overlay must
        // force network_access=true without erasing persona keys.
        let persona_cfg = r#"{"some_feature":{"enabled":true}}"#;
        // Config::from_args appends generated AFTER persona env vars.
        let extra = env(&[("CODEX_CONFIG", persona_cfg), ("CODEX_CONFIG", GENERATED)]);
        let merged = build_codex_config_env(&extra, None, true).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            v["some_feature"]["enabled"], true,
            "persona key must survive merge"
        );
        assert_eq!(
            v["sandbox_workspace_write"]["network_access"], true,
            "network_access must be forced true"
        );
    }

    #[test]
    fn build_codex_config_env_nested_persona_keys_survive_when_parent_has_same_top_level_key() {
        // Persona has sandbox_workspace_write.persona_only; parent has
        // sandbox_workspace_write.parent_only.  A flat top-level spread would drop
        // persona_only.  deep_merge must preserve both nested keys, and
        // network_access must be forced true last.
        let persona_cfg = r#"{"sandbox_workspace_write":{"persona_only":"keep_me"}}"#;
        let extra = env(&[("CODEX_CONFIG", persona_cfg), ("CODEX_CONFIG", GENERATED)]);
        let parent = r#"{"sandbox_workspace_write":{"parent_only":"also_here"}}"#;
        let merged = build_codex_config_env(&extra, Some(parent), true)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        // Both nested keys survive — no flat-spread drop.
        assert_eq!(
            v["sandbox_workspace_write"]["persona_only"], "keep_me",
            "nested persona key must survive when parent has the same top-level key"
        );
        assert_eq!(
            v["sandbox_workspace_write"]["parent_only"], "also_here",
            "nested parent key must be present"
        );
        // Forced last.
        assert_eq!(
            v["sandbox_workspace_write"]["network_access"], true,
            "network_access must be forced true"
        );
    }

    #[test]
    fn build_codex_config_env_parent_env_wins_on_collisions_persona_keys_survive() {
        // Parent env has CODEX_CONFIG with some keys; persona has different keys.
        // Parent wins on collision; unrelated persona keys survive.
        // network_access is always forced true.
        let persona_cfg = r#"{"persona_key":"persona_val","shared_key":"persona_version"}"#;
        // Config::from_args appends generated AFTER persona env vars.
        let extra = env(&[("CODEX_CONFIG", persona_cfg), ("CODEX_CONFIG", GENERATED)]);
        let parent = r#"{"parent_key":"parent_val","shared_key":"parent_version"}"#;
        let merged = build_codex_config_env(&extra, Some(parent), true)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        // Parent-only key present
        assert_eq!(
            v["parent_key"], "parent_val",
            "parent-only key must be present"
        );
        // Unrelated persona key survives (no collision with parent)
        assert_eq!(
            v["persona_key"], "persona_val",
            "unrelated persona key must survive"
        );
        // Collision: parent wins
        assert_eq!(
            v["shared_key"], "parent_version",
            "parent must win on colliding key"
        );
        // network_access always true (forced last)
        assert_eq!(v["sandbox_workspace_write"]["network_access"], true);
    }

    #[test]
    fn build_codex_config_env_parent_has_existing_sandbox_other_keys_survive() {
        // Parent env has sandbox_workspace_write with extra keys; after merge
        // those extra keys survive alongside network_access=true.
        let persona = r#"{}"#;
        let extra = env(&[("CODEX_CONFIG", persona), ("CODEX_CONFIG", GENERATED)]);
        let parent =
            r#"{"sandbox_workspace_write":{"network_access":false,"other_sandbox_key":"val"}}"#;
        let merged = build_codex_config_env(&extra, Some(parent), true)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        // network_access forced true even though parent set false
        assert_eq!(v["sandbox_workspace_write"]["network_access"], true);
        // other_sandbox_key survives (parent's sws merged, then network_access forced)
        assert_eq!(v["sandbox_workspace_write"]["other_sandbox_key"], "val");
    }

    #[test]
    fn build_codex_config_env_errors_on_invalid_persona_json() {
        // Bad persona JSON + generated overlay, signal=true → parse error before merging.
        let extra = env(&[("CODEX_CONFIG", "not-json"), ("CODEX_CONFIG", GENERATED)]);
        let result = build_codex_config_env(&extra, None, true);
        assert!(result.is_err(), "invalid persona JSON must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("CODEX_CONFIG"),
            "error must mention CODEX_CONFIG"
        );
    }

    #[test]
    fn build_codex_config_env_errors_on_non_object_persona_json() {
        // Non-object persona JSON + generated overlay, signal=true → parse error.
        let extra = env(&[("CODEX_CONFIG", "[1,2,3]"), ("CODEX_CONFIG", GENERATED)]);
        let result = build_codex_config_env(&extra, None, true);
        assert!(result.is_err(), "non-object persona JSON must return Err");
    }

    #[test]
    fn build_codex_config_env_errors_on_invalid_parent_json() {
        let persona = r#"{}"#;
        let extra = env(&[("CODEX_CONFIG", persona), ("CODEX_CONFIG", GENERATED)]);
        let result = build_codex_config_env(&extra, Some("bad-json"), true);
        assert!(result.is_err(), "invalid parent env JSON must return Err");
    }

    #[test]
    fn build_codex_config_env_errors_on_non_object_sandbox_workspace_write() {
        // sandbox_workspace_write must be an object for network_access forcing.
        // If the parent env sets it to a non-object scalar, deep_merge replaces
        // our object with the scalar, and the force step must fail clearly.
        let persona = r#"{}"#;
        let extra = env(&[("CODEX_CONFIG", persona), ("CODEX_CONFIG", GENERATED)]);
        // Parent replaces the object with a scalar — deep_merge: scalar overlay wins.
        let parent = r#"{"sandbox_workspace_write": 42}"#;
        let result = build_codex_config_env(&extra, Some(parent), true);
        assert!(
            result.is_err(),
            "non-object sandbox_workspace_write must return Err"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("sandbox_workspace_write"),
            "error must mention sandbox_workspace_write"
        );
    }
}
