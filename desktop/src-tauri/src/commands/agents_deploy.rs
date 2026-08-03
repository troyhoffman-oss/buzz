//! Provider deploy: payload construction and the deploy call itself, split
//! from `agents.rs` (file-size guard). The launch block is derived from the
//! same effective descriptor and policy helpers as local spawn so remote
//! execution does not reimplement them. `deploy_payload_json` is the pure
//! serialization half so payload completeness stays testable, and
//! `deploy_to_provider` resolves the binary, invokes it, and persists the
//! outcome onto the record.

use std::collections::BTreeMap;

use tauri::AppHandle;

#[cfg(test)]
use crate::managed_agents::AgentDefinition;
use crate::{
    app_state::AppState,
    managed_agents::{
        discover_provider_candidates, load_managed_agents, load_personas, provider_deploy,
        resolve_provider_binary, save_managed_agents, ManagedAgentRecord, ProviderFailure,
    },
    relay::relay_ws_url_with_override,
    util::now_iso,
};

/// Resolve the deploy-specific structured model/provider for a managed agent.
#[cfg(test)]
pub(crate) fn resolve_deploy_model_provider(
    record: &ManagedAgentRecord,
    personas: &[AgentDefinition],
    global: &crate::managed_agents::GlobalAgentConfig,
) -> (Option<String>, Option<String>) {
    crate::managed_agents::effective_config::resolve_effective_model_provider_pair(
        record, personas, global,
    )
    .unwrap_or((None, None))
}

/// Serialize the portable launch contract shared with provider-backed agents.
///
/// `descriptor.env` is the authoritative six-layer environment. Policy values
/// are deliberately separate because providers apply them below that layered
/// environment, preserving the local spawn's power-user override semantics.
pub(super) fn build_launch_block(
    record: &ManagedAgentRecord,
    descriptor: &crate::managed_agents::readiness::EffectiveHarnessDescriptor,
    teams: &[crate::managed_agents::TeamRecord],
    effective_prompt: Option<&str>,
    effective_model: Option<&str>,
    owner_pubkey: &str,
) -> serde_json::Value {
    use crate::managed_agents::{known_acp_runtime, resolve_session_title, SESSION_TITLE_ENV_VAR};

    let runtime = known_acp_runtime(&descriptor.command);
    let mut policy_env = BTreeMap::new();

    if let Some(runtime) = runtime {
        policy_env.extend(
            runtime
                .default_env
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
        );
        if runtime.mcp_hooks {
            policy_env.insert("MCP_HOOK_SERVERS".into(), "*".into());
        }
    }
    policy_env.insert("BUZZ_ACP_RELAY_OBSERVER".into(), "true".into());
    policy_env.insert("BUZZ_ACP_LAZY_POOL".into(), "true".into());
    policy_env.insert("BUZZ_ACP_AGENTS".into(), record.parallelism.to_string());

    if let Some(value) = effective_prompt {
        policy_env.insert("BUZZ_ACP_SYSTEM_PROMPT".into(), value.to_string());
    }
    if let Some(value) = effective_model {
        policy_env.insert("BUZZ_ACP_MODEL".into(), value.to_string());
    }
    if let Some(value) = record.idle_timeout_seconds {
        policy_env.insert("BUZZ_ACP_IDLE_TIMEOUT".into(), value.to_string());
    }
    if let Some(value) = record.max_turn_duration_seconds {
        policy_env.insert("BUZZ_ACP_MAX_TURN_DURATION".into(), value.to_string());
    }
    if let Some(value) = resolve_session_title(record.display_name.as_deref(), &record.name) {
        policy_env.insert(SESSION_TITLE_ENV_VAR.into(), value);
    }
    if let Some(value) =
        crate::managed_agents::spawn_hash::effective_team_instructions(record, teams)
    {
        policy_env.insert("BUZZ_ACP_TEAM_INSTRUCTIONS".into(), value);
    }

    serde_json::json!({
        "command": descriptor.command,
        "args": descriptor.args,
        "env": descriptor.env,
        "policy_env": policy_env,
        "owner_pubkey": owner_pubkey,
    })
}

pub(super) fn ensure_remote_provider_supported(provider: Option<&str>) -> Result<(), String> {
    if provider.map(str::trim) == Some(crate::managed_agents::RELAY_MESH_PROVIDER_ID) {
        return Err(
            "shared-compute agents cannot be deployed remotely because the mesh endpoint is local to the desktop"
                .to_string(),
        );
    }
    Ok(())
}

/// Build the standard agent JSON payload for provider deploy calls.
pub(super) fn build_deploy_payload(
    app: &AppHandle,
    state: &AppState,
    record: &ManagedAgentRecord,
) -> Result<serde_json::Value, String> {
    if let Some(err) = crate::managed_agents::spawn_key_refusal(record) {
        return Err(err);
    }

    let global = crate::managed_agents::load_global_agent_config(app).unwrap_or_default();
    let personas = load_personas(app).unwrap_or_default();
    let teams = crate::managed_agents::load_teams(app).unwrap_or_default();
    let persona_env =
        crate::managed_agents::live_persona_env(&personas, record.persona_id.as_deref());
    let global_persona_env = crate::managed_agents::merged_user_env(&global.env_vars, &persona_env);
    let merged_user_env =
        crate::managed_agents::merged_user_env(&global_persona_env, &record.env_vars);
    let effective = crate::managed_agents::effective_config::resolve_effective_config(
        record, &personas, &global,
    )
    .require_resolved()?;

    ensure_remote_provider_supported(effective.provider.value.as_deref())?;

    let descriptor =
        crate::managed_agents::resolve_effective_harness_descriptor(record, &personas, &global)
            .map_err(|error| crate::managed_agents::user_facing_harness_error(&error))?;
    let owner_pubkey = super::workspace_owner_hex(state)?;
    let launch = build_launch_block(
        record,
        &descriptor,
        &teams,
        effective.system_prompt.value.as_deref(),
        effective.model.value.as_deref(),
        &owner_pubkey,
    );

    Ok(deploy_payload_json(
        record,
        crate::relay::effective_agent_relay_url(
            &record.relay_url,
            &relay_ws_url_with_override(state),
        ),
        EffectiveDeployConfig {
            model: effective.model.value,
            provider: effective.provider.value,
            prompt: effective.system_prompt.value,
        },
        merged_user_env,
        launch,
        BinariesToPush::from_env(),
    ))
}

/// The binaries this machine offers to install on a host that resolves none.
///
/// A pair rather than two loose arguments because they are resolved together
/// and read together by the provider, and because passing them in at all is
/// what keeps [`deploy_payload_json`] pure.
#[derive(Default)]
pub(super) struct BinariesToPush {
    pub buzz_acp: Option<String>,
    pub buzz_cli: Option<String>,
}

impl BinariesToPush {
    /// The dogfood seams as this process's environment currently reports them.
    ///
    /// Each var names a Linux binary on THIS machine; `buzz-backend-ssh`
    /// streams it to the host inside the deploy script and installs it to
    /// `~/.local/bin` when the host resolves none. Read at deploy time rather
    /// than captured at startup, so a developer can point one at a fresh build
    /// without restarting the app.
    ///
    /// Two vars rather than one because the two binaries are separately
    /// policed: a host with no `buzz-acp` cannot run an agent at all, while a
    /// host with no CLI runs one that simply cannot reply with
    /// `buzz messages send`.
    ///
    /// They are env vars and not settings because the durable answer is not a
    /// path at all: the release build should resolve the artifact for the
    /// host's platform by version, with no user-visible choice. Until that
    /// lands this is the whole surface.
    fn from_env() -> Self {
        Self {
            buzz_acp: binary_to_push("BUZZ_ACP_PUSH_BINARY"),
            buzz_cli: binary_to_push("BUZZ_CLI_PUSH_BINARY"),
        }
    }
}

/// The three values `resolve_effective_config` resolves together.
///
/// Grouped for the same reason as [`BinariesToPush`]: they are produced by one
/// resolution step and read as one unit, so passing them as three loose
/// `Option<String>` invited transposing model for provider at a call site — all
/// three are the same type — and pushed the arity past clippy's limit.
pub(super) struct EffectiveDeployConfig {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub prompt: Option<String>,
}

/// Pure serialization half of [`build_deploy_payload`]. Legacy top-level fields
/// remain for display/bookkeeping; providers execute the resolved `launch` block.
pub(super) fn deploy_payload_json(
    record: &ManagedAgentRecord,
    relay_url: String,
    effective: EffectiveDeployConfig,
    merged_env: BTreeMap<String, String>,
    launch: serde_json::Value,
    binaries_to_push: BinariesToPush,
) -> serde_json::Value {
    let EffectiveDeployConfig {
        model: effective_model,
        provider: effective_provider,
        prompt: effective_prompt,
    } = effective;
    serde_json::json!({
        "name": &record.name,
        "relay_url": relay_url,
        "private_key_nsec": &record.private_key_nsec,
        "auth_tag": &record.auth_tag,
        "agent_command": &record.agent_command,
        "agent_args": &record.agent_args,
        "system_prompt": effective_prompt,
        "model": effective_model,
        "provider": effective_provider,
        "turn_timeout_seconds": record.turn_timeout_seconds,
        "idle_timeout_seconds": record.idle_timeout_seconds,
        "max_turn_duration_seconds": record.max_turn_duration_seconds,
        "parallelism": record.parallelism,
        "respond_to": record.respond_to,
        "respond_to_allowlist": &record.respond_to_allowlist,
        "env_vars": merged_env,
        "launch": launch,
        // A path on THIS machine to a Linux `buzz-acp` the provider may install
        // on the host when the host has none. Serialized as `null` when unset —
        // a provider reading it with `as_str()` sees `None`, exactly as it does
        // for an absent key, so behavior is unchanged by default.
        "buzz_acp_binary": binaries_to_push.buzz_acp,
        // The same, for the `buzz` CLI. A local agent gets the CLI because the
        // desktop bundles it as a sidecar and prepends its directory to the
        // spawned harness's PATH; a remote agent is told by the very same system
        // prompt to reply with `buzz messages send`, so without this the remote
        // half of that contract is missing. Unlike `buzz-acp` its absence is a
        // warning on the provider side, never a failed deploy.
        "buzz_cli_binary": binaries_to_push.buzz_cli,
    })
}

/// A push seam's value, or `None` when unset or blank — blank being a var the
/// user cleared rather than a request to push an empty path.
fn binary_to_push(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
}

/// Deploy an agent to a provider backend. Resolves the binary, calls deploy via
/// spawn_blocking, and persists the result (backend_agent_id or last_error).
///
/// Idempotency: calling deploy on an already-deployed agent sends the same payload
/// again. Providers are expected to handle this as an update-in-place or no-op —
/// the protocol does not include an explicit `undeploy` operation (deferred to v2).
///
/// Returns Ok(()) or a [`ProviderFailure`]; either way the record is updated
/// and saved first. The failure type is what keeps a recovery — a deploy onto a
/// tailnet wanting browser re-auth carries one — alive for callers that can
/// render it; each caller drops it explicitly where it cannot. The record's
/// `last_error` stays a plain string regardless: it is a post-mortem read long
/// after the fact, and an auth URL is a one-shot token that is stale by then.
/// Deploy an agent to a provider backend. Resolves the binary, calls deploy via
/// spawn_blocking, and persists the result (backend_agent_id or last_error).
///
/// Idempotency: calling deploy on an already-deployed agent sends the same payload
/// again. Providers are expected to handle this as an update-in-place or no-op —
/// the protocol does not include an explicit `undeploy` operation (deferred to v2).
///
/// Returns Ok(()) or a [`ProviderFailure`]; either way the record is updated
/// and saved first. The failure type is what keeps a recovery — a deploy onto a
/// tailnet wanting browser re-auth carries one — alive for callers that can
/// render it; each caller drops it explicitly where it cannot. The record's
/// `last_error` stays a plain string regardless: it is a post-mortem read long
/// after the fact, and an auth URL is a one-shot token that is stale by then.
pub(super) async fn deploy_to_provider(
    app: &AppHandle,
    state: &AppState,
    pubkey: &str,
    provider_id: &str,
    config: &serde_json::Value,
    agent_json: serde_json::Value,
    cached_binary_path: Option<&str>,
) -> Result<(), ProviderFailure> {
    // Resolve via discovered candidates only. Cached path must match BOTH
    // "is a discovered candidate" AND "belongs to this provider_id". A tampered
    // record cannot redirect deploys to a different provider's binary.
    let bin_path = cached_binary_path
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .map(|p| p.canonicalize().unwrap_or(p))
        .filter(|canonical| {
            discover_provider_candidates().iter().any(|(id, cp)| {
                id == provider_id && cp.canonicalize().ok().as_ref() == Some(canonical)
            })
        })
        .map_or_else(|| resolve_provider_binary(provider_id), Ok)?;

    let config_clone = config.clone();
    let deploy_result =
        tokio::task::spawn_blocking(move || provider_deploy(&bin_path, &agent_json, &config_clone))
            .await
            .map_err(|e| format!("spawn_blocking failed: {e}"))?;

    // Persist result under lock.
    let _store_guard = state
        .managed_agents_store_lock
        .lock()
        .map_err(|e| e.to_string())?;
    let mut records = load_managed_agents(app)?;
    let rec = records
        .iter_mut()
        .find(|r| r.pubkey == pubkey)
        .ok_or_else(|| format!("agent {pubkey} not found"))?;

    match deploy_result {
        Ok(backend_agent_id) => {
            rec.backend_agent_id = Some(backend_agent_id);
            rec.last_started_at = Some(now_iso());
            rec.updated_at = now_iso();
            rec.last_error = None;
        }
        Err(ref e) => {
            // The message only — see the `last_error` note above.
            rec.last_error = Some(e.message.clone());
            rec.updated_at = now_iso();
            save_managed_agents(app, &records)?;
            return Err(e.clone());
        }
    }
    save_managed_agents(app, &records)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_agents::{readiness::EffectiveHarnessDescriptor, RespondTo, TeamRecord};

    fn record() -> ManagedAgentRecord {
        serde_json::from_value(serde_json::json!({
            "pubkey": "abcd1234",
            "name": "agent-handle",
            "display_name": "Agent\u{0000} Name",
            "private_key_nsec": "nsec1fake",
            "relay_url": "wss://relay.example",
            "acp_command": "buzz-acp",
            "agent_command": "goose",
            "agent_args": [],
            "mcp_command": "",
            "turn_timeout_seconds": 320,
            "idle_timeout_seconds": 17,
            "max_turn_duration_seconds": 23,
            "parallelism": 4,
            "respond_to": RespondTo::OwnerOnly,
            "respond_to_allowlist": [],
            "team_id": "team-1",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn launch_block_preserves_descriptor_and_spawn_policy() {
        let record = record();
        let descriptor = EffectiveHarnessDescriptor {
            command: "goose".into(),
            args: vec!["acp".into()],
            env: BTreeMap::from([
                ("GOOSE_MODE".into(), "custom".into()),
                ("SECRET_FROM_PERSONA".into(), "secret".into()),
            ]),
        };
        let teams: Vec<TeamRecord> = serde_json::from_value(serde_json::json!([{
            "id": "team-1", "name": "Team", "instructions": "Coordinate", "persona_ids": [], "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }])).unwrap();

        let launch = build_launch_block(
            &record,
            &descriptor,
            &teams,
            Some("prompt"),
            Some("model"),
            "owner-hex",
        );

        assert_eq!(launch["command"], "goose");
        assert_eq!(launch["args"], serde_json::json!(["acp"]));
        assert_eq!(launch["env"]["GOOSE_MODE"], "custom");
        // policy_env is applied first, so this default remains separate from
        // the descriptor value that wins in launch.env.
        assert_eq!(launch["policy_env"]["GOOSE_MODE"], "auto");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_LAZY_POOL"], "true");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_RELAY_OBSERVER"], "true");
        assert_eq!(
            launch["policy_env"]["BUZZ_ACP_TEAM_INSTRUCTIONS"],
            "Coordinate"
        );
        assert_eq!(launch["policy_env"]["BUZZ_ACP_SESSION_TITLE"], "Agent Name");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_SYSTEM_PROMPT"], "prompt");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_MODEL"], "model");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_IDLE_TIMEOUT"], "17");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_MAX_TURN_DURATION"], "23");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_AGENTS"], "4");
        assert_eq!(launch["owner_pubkey"], "owner-hex");
    }
}
