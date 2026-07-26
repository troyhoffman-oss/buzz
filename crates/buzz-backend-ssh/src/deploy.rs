//! `deploy`: provision the agent as a `systemd --user` unit on the host.
//!
//! `--user` rather than a system unit keeps the flow root-free and puts the env
//! file beside the harness credentials that already live in the deploying
//! user's home (`~/.claude`, `~/.config/goose`).
//!
//! Deploy is also the *start* path — `start_managed_agent` re-enters
//! `deploy_to_provider` — so everything here must be idempotent.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::install::{self, Payload};
use crate::protocol::{Secret, SshConfig};
use crate::ssh::{quote, Session};

/// The templated unit, installed once per host and instantiated per agent.
const UNIT_TEMPLATE: &str = include_str!("../assets/buzz-acp@.service");

/// Verbatim copy of the desktop's `env_vars::RESERVED_ENV_KEYS`. The desktop
/// already strips these from user env; re-checking here means a leak needs two
/// independent failures rather than one, and this binary ships and updates
/// separately from the desktop that fills the payload.
const RESERVED_ENV_KEYS: &[&str] = &[
    "BUZZ_PRIVATE_KEY",
    "NOSTR_PRIVATE_KEY",
    "BUZZ_AUTH_TAG",
    "BUZZ_API_TOKEN",
    "BUZZ_ACP_PRIVATE_KEY",
    "BUZZ_ACP_API_TOKEN",
    "BUZZ_RELAY_URL",
    "BUZZ_ACP_AGENT_COMMAND",
    "BUZZ_ACP_AGENT_ARGS",
    "BUZZ_ACP_MCP_COMMAND",
    "BUZZ_ACP_RESPOND_TO",
    "BUZZ_ACP_RESPOND_TO_ALLOWLIST",
    "BUZZ_ACP_AGENT_OWNER",
    "BUZZ_ACP_SETUP_PAYLOAD",
    "BUZZ_MANAGED_AGENT",
    "BUZZ_MANAGED_AGENT_START_NONCE",
];

/// The deploy payload, as `deploy_payload_json` serializes it.
///
/// Deliberately not `Debug`. `Secret` redacts itself, but `env_vars` routinely
/// holds `ANTHROPIC_API_KEY` and friends in plain `String`s, so a derived
/// `Debug` would put provider credentials one `{:?}` away from a log line.
pub struct Agent {
    pub name: String,
    pub relay_url: String,
    pub private_key_nsec: Secret,
    pub auth_tag: Option<String>,
    /// The pinned harness command. See [`Agent::from_request`].
    pub agent_command: String,
    pub agent_args: Vec<String>,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub idle_timeout_seconds: Option<u64>,
    pub max_turn_duration_seconds: Option<u64>,
    pub parallelism: u64,
    pub respond_to: String,
    pub respond_to_allowlist: Vec<String>,
    pub env_vars: BTreeMap<String, String>,
    /// A path on the **desktop** machine to a Linux `buzz-acp` to install on
    /// the host when the host resolves none. Optional, and absent it changes
    /// nothing: deploy resolves `buzz-acp` on the host or fails with exit 90
    /// exactly as it always has. See [`crate::install`].
    pub buzz_acp_binary: Option<String>,
}

impl Agent {
    pub fn from_request(request: &serde_json::Value) -> Result<Self, String> {
        let agent = request.get("agent").ok_or("request is missing 'agent'")?;
        let string = |key: &str| {
            agent
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };

        let private_key_nsec: Secret = agent
            .get("private_key_nsec")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|_| "'private_key_nsec' must be a string".to_string())?
            .unwrap_or_default();
        // Fail closed. A deploy that lets the host mint its own key produces an
        // agent that looks deployed and is permanently unreachable: presence,
        // mentions, `!shutdown`, badges and the NIP-OA auth tag all key off the
        // pubkey the desktop minted. Mirrors the desktop's own
        // `spawn_key_refusal`.
        if private_key_nsec.is_empty() {
            return Err(
                "refusing to deploy without the agent's minted private key: the remote agent \
                 would run under an identity no desktop surface recognizes"
                    .to_string(),
            );
        }

        // The remote harness choice reaches the host ONLY as this pin — the
        // desktop resolves it from the remote catalog at create time and ships
        // it verbatim. A blank value means the pin was lost on the way, and the
        // host would silently run `buzz-agent` instead of the harness the user
        // picked, so refuse rather than substitute.
        let agent_command = string("agent_command").ok_or(
            "deploy payload carries no 'agent_command': the harness pin was lost before it \
             reached the host (see instanceInputForDefinition provider branch)",
        )?;

        Ok(Self {
            name: string("name").ok_or("'name' is required")?,
            relay_url: string("relay_url").ok_or("'relay_url' is required")?,
            private_key_nsec,
            auth_tag: string("auth_tag"),
            agent_command,
            // `agent_args` must be the remote entry's default args. The
            // desktop's local branch sends `[]` on purpose so spawn re-resolves
            // them live, but a provider-backed record never spawns locally, so
            // `[]` here would mean "no args" for any harness the local
            // default-args table does not know.
            agent_args: crate::discover::string_list(agent.get("agent_args")),
            system_prompt: string("system_prompt"),
            model: string("model"),
            provider: string("provider"),
            // `turn_timeout_seconds` is deliberately not read: the payload still
            // carries it, but `BUZZ_ACP_TURN_TIMEOUT` is deprecated and ignored
            // by the harness (`buzz-acp::config`), and local spawn does not
            // write it either. `idle_timeout_seconds` and
            // `max_turn_duration_seconds` are the live controls.
            idle_timeout_seconds: agent.get("idle_timeout_seconds").and_then(|v| v.as_u64()),
            max_turn_duration_seconds: agent
                .get("max_turn_duration_seconds")
                .and_then(|v| v.as_u64()),
            parallelism: agent
                .get("parallelism")
                .and_then(|v| v.as_u64())
                .filter(|p| *p > 0)
                .unwrap_or(1),
            respond_to: string("respond_to").unwrap_or_else(|| "owner-only".to_string()),
            respond_to_allowlist: crate::discover::string_list(agent.get("respond_to_allowlist")),
            env_vars: env_map(agent.get("env_vars")),
            // Read from the same `agent` block as everything else, but it is
            // not agent configuration: nothing about it reaches the env file or
            // the unit. It is the desktop handing the provider a copy of
            // `buzz-acp` to install if the host turns out not to have one.
            buzz_acp_binary: string("buzz_acp_binary"),
        })
    }

    /// The systemd instance name, and the `agent_id` the desktop persists in
    /// `record.backend_agent_id`.
    ///
    /// It becomes both a filename and a unit instance name, so it follows the
    /// desktop's own `util::slugify` rule. The hash suffix is not decoration:
    /// the payload carries no stable agent identifier, and without it two
    /// agents whose names differ only in punctuation would share one unit and
    /// one env file.
    pub fn slug(&self) -> String {
        let sanitized: String = self
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        // ASCII by construction, so a byte slice cannot split a character.
        let stem = sanitized.trim_matches('-');
        let stem = &stem[..stem.len().min(32)];
        let stem = stem.trim_end_matches('-');
        let stem = if stem.is_empty() { "agent" } else { stem };
        format!("{stem}-{}", short_hash(&self.name))
    }

    pub fn agent_id(&self) -> String {
        format!("buzz-acp@{}", self.slug())
    }
}

/// FNV-1a, truncated. Only ever used to keep distinct names on distinct units.
fn short_hash(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash as u32)
}

pub fn env_map(value: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// A well-formed POSIX env var name. Mirrors the desktop's own boundary check;
/// a malformed key would let a value smuggle an extra assignment into the file,
/// or — on the left side of a shell `export`, where quoting cannot help — an
/// extra command.
pub fn is_well_formed_env_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One `KEY="value"` line for a systemd `EnvironmentFile`.
///
/// systemd unquotes C-style escapes inside double quotes, so `\` and `"` are
/// the two characters that must be escaped. Control characters are refused
/// outright: a newline would split the assignment into a second, attacker-
/// chosen line.
fn env_line(key: &str, value: &str) -> Result<String, String> {
    if value.chars().any(|c| c.is_control()) {
        return Err(format!(
            "env var '{key}' contains a control character and cannot be written to the unit's \
             environment file"
        ));
    }
    let escaped = value.replace('\\', r"\\").replace('"', "\\\"");
    Ok(format!("{key}=\"{escaped}\"\n"))
}

/// The env file body: the local spawn contract from `runtime.rs`, transcribed.
///
/// Values resolved on the host — the absolute harness path, `buzz-acp` itself,
/// `git-credential-nostr`, `PATH` — are appended by the remote script, not
/// here. Everything in this string is known locally.
fn env_file_body(agent: &Agent) -> Result<String, String> {
    let mut body = String::new();
    let mut push = |key: &str, value: &str| -> Result<(), String> {
        body.push_str(&env_line(key, value)?);
        Ok(())
    };

    push("BUZZ_PRIVATE_KEY", agent.private_key_nsec.expose())?;
    push("BUZZ_RELAY_URL", &agent.relay_url)?;
    if let Some(auth_tag) = &agent.auth_tag {
        push("BUZZ_AUTH_TAG", auth_tag)?;
    }
    push("BUZZ_ACP_AGENT_ARGS", &agent.agent_args.join(","))?;
    // MCP does not reach the host yet: `mcp_command` is local catalog metadata,
    // and mirroring that table here would drift. Empty rather than omitted,
    // matching what local spawn writes when it does not apply.
    push("BUZZ_ACP_MCP_COMMAND", "")?;
    // `BUZZ_ACP_LAZY_POOL=true` is the desktop's lazy pair-start concept, which
    // has no meaning for a unit systemd starts unconditionally. Written
    // explicitly so the harness default cannot drift underneath us.
    push("BUZZ_ACP_LAZY_POOL", "false")?;
    push("BUZZ_ACP_AGENTS", &agent.parallelism.to_string())?;
    push("BUZZ_ACP_MULTIPLE_EVENT_HANDLING", "steer")?;
    push("BUZZ_ACP_DEDUP", "queue")?;
    push("BUZZ_ACP_RELAY_OBSERVER", "true")?;
    push("BUZZ_ACP_RESPOND_TO", &agent.respond_to)?;
    if agent.respond_to == "allowlist" {
        if agent.respond_to_allowlist.is_empty() {
            return Err(
                "respond-to mode 'allowlist' requires at least one pubkey in the allowlist"
                    .to_string(),
            );
        }
        push(
            "BUZZ_ACP_RESPOND_TO_ALLOWLIST",
            &agent.respond_to_allowlist.join(","),
        )?;
    }
    if let Some(prompt) = &agent.system_prompt {
        push("BUZZ_ACP_SYSTEM_PROMPT", prompt)?;
    }
    if let Some(model) = &agent.model {
        push("BUZZ_ACP_MODEL", model)?;
    }
    // The harness-native half of the same selection: `BUZZ_ACP_MODEL` is what
    // buzz-acp reads, these are what the harness underneath it reads, and local
    // spawn writes both.
    for (key, value) in metadata_env(agent) {
        push(key, value)?;
    }
    // Only when the user set them, so the harness's own defaults win otherwise.
    if let Some(idle) = agent.idle_timeout_seconds {
        push("BUZZ_ACP_IDLE_TIMEOUT", &idle.to_string())?;
    }
    if let Some(max_turn) = agent.max_turn_duration_seconds {
        push("BUZZ_ACP_MAX_TURN_DURATION", &max_turn.to_string())?;
    }

    // `BUZZ_MANAGED_AGENT` is deliberately absent: it is the desktop's marker
    // for reclaiming orphaned local children, and systemd owns this lifecycle.

    // User env last, so it overrides everything above — systemd applies the
    // later assignment for a repeated key, matching the local layering.
    for (key, value) in &agent.env_vars {
        if !is_well_formed_env_key(key) {
            return Err(format!("env var name '{key}' is not a valid identifier"));
        }
        if RESERVED_ENV_KEYS
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(key))
        {
            return Err(format!(
                "env var '{key}' is reserved and cannot be overridden"
            ));
        }
        push(key, value)?;
    }
    Ok(body)
}

/// The remote half of `runtime_metadata_env_vars` (`runtime.rs`).
///
/// Local spawn writes the effective model and provider into each runtime's own
/// `model_env_var` / `provider_env_var`. Without this a remote Goose would see
/// `BUZZ_ACP_MODEL` but no `GOOSE_MODEL`, and fall back to whatever
/// `~/.config/goose/config.yaml` on the host says — the user's model pick
/// silently ignored.
///
/// Keyed by command rather than harness id because the id is a create-time
/// desktop concept and the env file is written from the pin. Runtimes absent
/// here (Claude, Codex) declare no such vars in `KNOWN_ACP_RUNTIMES` either.
fn metadata_env(agent: &Agent) -> Vec<(&'static str, &str)> {
    const RUNTIME_ENV: &[(&str, &str, &str)] = &[
        ("goose", "GOOSE_MODEL", "GOOSE_PROVIDER"),
        ("buzz-agent", "BUZZ_AGENT_MODEL", "BUZZ_AGENT_PROVIDER"),
    ];

    // The pin may be a bare name or an absolute path; local spawn's
    // `known_acp_runtime` matches on the file name either way.
    let command = agent
        .agent_command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&agent.agent_command);
    let Some((_, model_key, provider_key)) =
        RUNTIME_ENV.iter().find(|(name, _, _)| *name == command)
    else {
        return Vec::new();
    };

    [
        (*model_key, agent.model.as_deref()),
        (*provider_key, agent.provider.as_deref()),
    ]
    .into_iter()
    .filter_map(|(key, value)| Some((key, value?)))
    .collect()
}

/// `wss://relay` → `https://relay`, for the git credential helper's scope.
fn relay_http_base_url(relay_url: &str) -> String {
    let trimmed = relay_url.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    }
}

/// The remote script. One round trip: resolve (or install), write, install,
/// start.
///
/// Every secret reaches the host inside this script, which travels on the SSH
/// stdin channel. Nothing secret is ever an argument — not to `ssh`, and not to
/// any command the script runs — because the remote `ps` is world-readable. The
/// env file is written under `umask 077`, `chmod 600`, and moved into place
/// atomically.
///
/// `push` is the optional desktop-side `buzz-acp`. It is resolved *first*, so
/// `$acp` — and therefore the unit's `ExecStart` — names the copy this same
/// pass installed. The pushed bytes are not secret, but they share the stream
/// with the minted nsec, so they travel base64-encoded and never as raw bytes
/// (`install`).
fn deploy_script(
    agent: &Agent,
    config: &SshConfig,
    unit: &str,
    push: Option<&Payload>,
) -> Result<String, String> {
    let slug = agent.slug();
    let acp = quote(config.buzz_acp_path.as_deref().unwrap_or("buzz-acp"));
    let command = quote(&agent.agent_command);
    let relay_http = relay_http_base_url(&agent.relay_url);
    let resolve_acp = install::resolve_or_install(&acp, push);

    let mut script = String::from("set -eu\numask 077\n");
    // The harness name is bound once and thereafter referenced only as
    // `"$harness_name"`. Interpolating it into the double-quoted error message
    // would be a command-injection hole: `quote()` makes a value inert as an
    // *argument*, but inside double quotes its single quotes are literal and a
    // `$(...)` would still run. Expansion results are not re-scanned.
    script.push_str(&format!(
        r#"harness_name={command}
{resolve_acp}
harness=$(command -v "$harness_name" 2>/dev/null) || {{ echo "harness $harness_name not found on the server's PATH" >&2; exit 91; }}
cred=$(command -v git-credential-nostr 2>/dev/null || true)
conf="$HOME/.config/buzz-acp"
units="$HOME/.config/systemd/user"
mkdir -p "$conf" "$units"
env_file="$conf/{slug}.env"
tmp="$env_file.new"
"#
    ));

    // No body line can terminate the heredoc: every line is `KEY="..."` and
    // `env_line` refuses control characters. The quoted delimiter suppresses
    // expansion, so a value is never interpreted by the shell.
    script.push_str("{\n");
    script.push_str("printf 'BUZZ_ACP_AGENT_COMMAND=\"%s\"\\n' \"$harness\"\n");
    script.push_str("printf 'PATH=\"%s\"\\n' \"$HOME/.local/bin:$PATH\"\n");
    script.push_str("cat <<'BUZZ_ENV_EOF'\n");
    script.push_str(&env_file_body(agent)?);
    script.push_str("BUZZ_ENV_EOF\n");
    // Git over the relay's NIP-98 endpoint, only when the helper is installed.
    // NOSTR_PRIVATE_KEY mirrors BUZZ_PRIVATE_KEY, as it does locally.
    let helper_key = format!("credential.{relay_http}/git.helper");
    let use_http_path_key = format!("credential.{relay_http}/git.useHttpPath");
    let git_block: String = [
        ("NOSTR_PRIVATE_KEY", agent.private_key_nsec.expose()),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", helper_key.as_str()),
        ("GIT_CONFIG_KEY_1", use_http_path_key.as_str()),
        ("GIT_CONFIG_VALUE_1", "true"),
    ]
    .into_iter()
    .map(|(key, value)| env_line(key, value))
    .collect::<Result<_, _>>()?;
    script.push_str(&format!(
        r#"if [ -n "$cred" ]; then
printf 'GIT_CONFIG_VALUE_0="%s"\n' "$cred"
cat <<'BUZZ_GIT_EOF'
{git_block}BUZZ_GIT_EOF
fi
}} > "$tmp"
chmod 600 "$tmp"
mv "$tmp" "$env_file"
"#
    ));

    // Without lingering the agent dies when this SSH session ends, which reads
    // as a flaky agent rather than a configuration problem. It also creates
    // `/run/user/$(id -u)`, so it must precede anything that talks to the user
    // bus. Best-effort: some hosts gate it behind polkit, and failing it must
    // not fail an otherwise good deploy.
    //
    // A non-interactive SSH command often gets no `XDG_RUNTIME_DIR`, without
    // which every `systemctl --user` fails with "Failed to connect to bus".
    script.push_str(
        r#"loginctl enable-linger "$(id -un)" >/dev/null 2>&1 || true
if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
  XDG_RUNTIME_DIR="/run/user/$(id -u)"
  export XDG_RUNTIME_DIR
fi
"#,
    );

    // Install the templated unit, reloading only when its content changed — a
    // `daemon-reload` per start is noise, and a missing one after a change
    // silently runs the old unit.
    //
    // `@BUZZ_ACP_BIN@` is substituted with parameter expansion rather than
    // `sed`: `sed -i` is a GNU extension BSD and macOS hosts reject, and any
    // `s///` would need a delimiter no resolved path can contain.
    script.push_str(&format!(
        r#"unit_file="$units/buzz-acp@.service"
template=$(cat <<'BUZZ_UNIT_EOF'
{unit}BUZZ_UNIT_EOF
)
printf '%s\n' "${{template%%@BUZZ_ACP_BIN@*}}$acp${{template#*@BUZZ_ACP_BIN@}}" > "$unit_file.new"
if cmp -s "$unit_file.new" "$unit_file"; then
  rm -f "$unit_file.new"
else
  mv "$unit_file.new" "$unit_file"
  systemctl --user daemon-reload
fi
systemctl --user enable --now {service} >/dev/null
# Redeploy is also the start path, so an already-running unit must pick up the
# rewritten env file rather than be left on the old one.
systemctl --user restart {service}
"#,
        service = quote(&format!("buzz-acp@{slug}.service")),
    ));
    Ok(script)
}

/// The binary to embed in this deploy's script, if any.
///
/// `None` whenever the payload carries no path — the default, and the case in
/// which nothing about deploy changes. Otherwise the host is asked first
/// whether it already resolves `buzz-acp`, because **deploy is the start path**:
/// without the probe, a desktop with the seam engaged would encode and stream
/// tens of megabytes on every agent start, forever, to a host that has had the
/// binary since the first deploy. Reading the file is skipped in that case too.
///
/// A probe that cannot be answered is not fatal: the binary is embedded and the
/// script's own `command -v` makes the real decision on the host.
fn payload_to_push(
    agent: &Agent,
    config: &SshConfig,
    session: &Session,
) -> Result<Option<Payload>, String> {
    let Some(path) = agent.buzz_acp_binary.as_deref() else {
        return Ok(None);
    };
    let acp = quote(config.buzz_acp_path.as_deref().unwrap_or("buzz-acp"));
    let probe = session.run(&install::probe_script(&acp), Duration::from_secs(60))?;
    if probe.ok() {
        return Ok(None);
    }
    // Read and validate before the deploy script is built: a bad path, a non-ELF
    // file or an oversized one is the desktop's mistake, and it should be
    // reported as that rather than as a remote failure mid-provisioning.
    Payload::read(path).map(Some)
}

pub fn deploy(
    request: &serde_json::Value,
    config: &SshConfig,
    session: &Session,
) -> Result<serde_json::Value, String> {
    let agent = Agent::from_request(request)?;
    let push = payload_to_push(&agent, config, session)?;
    let script = deploy_script(&agent, config, UNIT_TEMPLATE, push.as_ref())?;
    let output = session.run(&script, Duration::from_secs(300))?;
    if !output.ok() {
        return Err(output.failure());
    }
    Ok(serde_json::json!({ "ok": true, "agent_id": agent.agent_id() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NSEC: &str = "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";

    fn request() -> serde_json::Value {
        serde_json::json!({
            "op": "deploy",
            "provider_config": { "ssh_host": "vps", "ssh_user": "ubuntu" },
            "agent": {
                "name": "Research Bot",
                "relay_url": "wss://relay.example/ws",
                "private_key_nsec": NSEC,
                "auth_tag": "tag-abc",
                "agent_command": "goose",
                "agent_args": ["acp"],
                "system_prompt": "be brief",
                "model": "claude-sonnet-5",
                "provider": "anthropic",
                "parallelism": 3,
                "respond_to": "owner-only",
                "respond_to_allowlist": [],
                "env_vars": { "ANTHROPIC_API_KEY": "sk-ant-secret" },
            },
        })
    }

    fn config() -> SshConfig {
        SshConfig {
            host: "vps".into(),
            ..SshConfig::default()
        }
    }

    /// `Agent` is intentionally not `Debug` (see its doc comment), so tests
    /// unwrap the error by hand rather than through `unwrap_err`.
    fn rejection(request: &serde_json::Value) -> String {
        match Agent::from_request(request) {
            Err(error) => error,
            Ok(agent) => panic!("expected a rejection, got agent {}", agent.agent_id()),
        }
    }

    #[test]
    fn deploy_fails_closed_without_the_minted_key() {
        let mut request = request();
        request["agent"]["private_key_nsec"] = serde_json::json!("");
        let error = rejection(&request);
        assert!(error.contains("minted private key"), "{error}");

        request["agent"]
            .as_object_mut()
            .unwrap()
            .remove("private_key_nsec");
        assert!(rejection(&request).contains("minted private key"));
    }

    #[test]
    fn deploy_refuses_a_payload_whose_harness_pin_was_lost() {
        // Without the pin the host would fall back to `buzz-agent` and the
        // user's harness choice would vanish silently.
        let mut request = request();
        request["agent"]["agent_command"] = serde_json::json!("");
        let error = rejection(&request);
        assert!(error.contains("harness pin"), "{error}");
    }

    #[test]
    fn the_pinned_harness_is_what_the_unit_runs() {
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        // Resolved to an absolute path on the HOST, and written into the env
        // file systemd re-reads on every restart — that is what makes the pin
        // durable rather than a one-shot argument.
        assert!(script.contains("harness_name='goose'"));
        assert!(script.contains(r#"harness=$(command -v "$harness_name""#));
        assert!(script.contains("printf 'BUZZ_ACP_AGENT_COMMAND=\"%s\"\\n' \"$harness\""));
        assert!(script.contains(r#"BUZZ_ACP_AGENT_ARGS="acp""#));
        // And a missing harness is a failure, never a substitution.
        assert!(script.contains("exit 91"));
    }

    /// A Hermes per-profile pin, end to end through the deploy path.
    ///
    /// `discover_harnesses` emits `["--profile", <name>, "acp"]`, and the args
    /// reach the host as ONE comma-joined `BUZZ_ACP_AGENT_ARGS` that `buzz-acp`
    /// re-splits on `,` (`config.rs`, `value_delimiter`). That round trip is
    /// only lossless because a profile name cannot contain a comma — which is
    /// what `is_hermes_profile_name` guarantees — so pin the whole chain here
    /// rather than trusting the two halves independently.
    #[test]
    fn a_hermes_profile_pin_reaches_the_host_intact() {
        let mut request = request();
        request["agent"]["agent_command"] = serde_json::json!("hermes");
        request["agent"]["agent_args"] =
            serde_json::json!(["--profile", "msig-web-analyst", "acp"]);
        let agent = Agent::from_request(&request).unwrap();
        assert_eq!(
            agent.agent_args,
            ["--profile", "msig-web-analyst", "acp"],
            "provider args are pinned verbatim, never re-resolved"
        );

        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        assert!(script.contains("harness_name='hermes'"));
        assert!(
            script.contains(r#"BUZZ_ACP_AGENT_ARGS="--profile,msig-web-analyst,acp""#),
            "{script}"
        );
    }

    #[test]
    fn secrets_travel_in_the_script_body_and_never_on_an_argv() {
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        assert!(script.contains(&format!("BUZZ_PRIVATE_KEY=\"{NSEC}\"")));
        assert!(script.contains(&format!("NOSTR_PRIVATE_KEY=\"{NSEC}\"")));
        assert!(script.contains("ANTHROPIC_API_KEY=\"sk-ant-secret\""));

        // Every secret-bearing line lives inside a quoted heredoc, so the
        // remote shell never expands it and it never becomes an argument to
        // anything. The commands the script *runs* carry no secret at all.
        for line in script.lines() {
            if line.contains(NSEC) || line.contains("sk-ant-secret") {
                assert!(
                    line.starts_with("BUZZ_")
                        || line.starts_with("NOSTR_")
                        || line.starts_with("ANTHROPIC_"),
                    "secret escaped the heredoc body: {line}"
                );
            }
        }
        assert!(script.contains("umask 077"));
        assert!(script.contains("chmod 600 \"$tmp\""));
    }

    #[test]
    fn the_env_file_transcribes_the_local_spawn_contract() {
        let agent = Agent::from_request(&request()).unwrap();
        let body = env_file_body(&agent).unwrap();
        for expected in [
            r#"BUZZ_RELAY_URL="wss://relay.example/ws""#,
            r#"BUZZ_AUTH_TAG="tag-abc""#,
            r#"BUZZ_ACP_LAZY_POOL="false""#,
            r#"BUZZ_ACP_AGENTS="3""#,
            r#"BUZZ_ACP_MULTIPLE_EVENT_HANDLING="steer""#,
            r#"BUZZ_ACP_DEDUP="queue""#,
            r#"BUZZ_ACP_RELAY_OBSERVER="true""#,
            r#"BUZZ_ACP_RESPOND_TO="owner-only""#,
            r#"BUZZ_ACP_SYSTEM_PROMPT="be brief""#,
            r#"BUZZ_ACP_MODEL="claude-sonnet-5""#,
            r#"BUZZ_ACP_MCP_COMMAND="""#,
        ] {
            assert!(body.contains(expected), "missing {expected}");
        }
        // Local process-ownership marker: meaningless where systemd owns the
        // lifecycle, so it is never written.
        assert!(!body.contains("BUZZ_MANAGED_AGENT"));
        // Unset timeouts are omitted so the harness's own defaults win.
        assert!(!body.contains("BUZZ_ACP_IDLE_TIMEOUT"));
        assert!(!body.contains("BUZZ_ACP_MAX_TURN_DURATION"));
        // No allowlist key unless the mode asks for one.
        assert!(!body.contains("BUZZ_ACP_RESPOND_TO_ALLOWLIST"));
    }

    #[test]
    fn the_harness_sees_the_same_model_env_local_spawn_would_set() {
        // `runtime_metadata_env_vars` parity: buzz-acp reads BUZZ_ACP_MODEL,
        // but Goose itself reads GOOSE_MODEL/GOOSE_PROVIDER. Emitting only the
        // former leaves the host's ~/.config/goose/config.yaml deciding the
        // model, silently overriding the user's pick.
        let body = env_file_body(&Agent::from_request(&request()).unwrap()).unwrap();
        assert!(body.contains(r#"GOOSE_MODEL="claude-sonnet-5""#), "{body}");
        assert!(body.contains(r#"GOOSE_PROVIDER="anthropic""#), "{body}");

        // An absolute pin resolves to the same runtime — `known_acp_runtime`
        // matches on the file name locally, so this must too.
        let mut request = request();
        request["agent"]["agent_command"] = serde_json::json!("/home/ubuntu/.local/bin/goose");
        let body = env_file_body(&Agent::from_request(&request).unwrap()).unwrap();
        assert!(body.contains(r#"GOOSE_MODEL="claude-sonnet-5""#), "{body}");

        // Runtimes that declare no model/provider env upstream get none here.
        request["agent"]["agent_command"] = serde_json::json!("claude-code-acp");
        let body = env_file_body(&Agent::from_request(&request).unwrap()).unwrap();
        assert!(!body.contains("GOOSE_MODEL"));
        assert!(body.contains(r#"BUZZ_ACP_MODEL="claude-sonnet-5""#));

        // And an unset field writes no key at all, so the harness default wins.
        request["agent"]["agent_command"] = serde_json::json!("goose");
        request["agent"]["provider"] = serde_json::json!("");
        let body = env_file_body(&Agent::from_request(&request).unwrap()).unwrap();
        assert!(body.contains("GOOSE_MODEL"));
        assert!(!body.contains("GOOSE_PROVIDER"));
    }

    #[test]
    fn the_deprecated_turn_timeout_is_never_written() {
        // The payload still carries `turn_timeout_seconds` (upstream
        // `deploy_payload_json`), but `BUZZ_ACP_TURN_TIMEOUT` is deprecated and
        // ignored by the harness, and local spawn does not write it either.
        let mut request = request();
        request["agent"]["turn_timeout_seconds"] = serde_json::json!(320);
        let body = env_file_body(&Agent::from_request(&request).unwrap()).unwrap();
        assert!(!body.contains("TURN_TIMEOUT"), "{body}");
    }

    #[test]
    fn timeouts_are_emitted_only_when_set() {
        let mut request = request();
        request["agent"]["idle_timeout_seconds"] = serde_json::json!(900);
        request["agent"]["max_turn_duration_seconds"] = serde_json::json!(3600);
        let body = env_file_body(&Agent::from_request(&request).unwrap()).unwrap();
        assert!(body.contains(r#"BUZZ_ACP_IDLE_TIMEOUT="900""#));
        assert!(body.contains(r#"BUZZ_ACP_MAX_TURN_DURATION="3600""#));
    }

    #[test]
    fn user_env_is_written_last_so_it_overrides() {
        let mut request = request();
        request["agent"]["env_vars"] = serde_json::json!({ "GOOSE_MODE": "auto" });
        let body = env_file_body(&Agent::from_request(&request).unwrap()).unwrap();
        let user = body.find("GOOSE_MODE").unwrap();
        assert!(body.find("BUZZ_ACP_MODEL").unwrap() < user);
    }

    #[test]
    fn reserved_and_malformed_env_keys_are_refused() {
        for key in ["BUZZ_PRIVATE_KEY", "buzz_relay_url", "BUZZ_MANAGED_AGENT"] {
            let mut request = request();
            request["agent"]["env_vars"] = serde_json::json!({ key: "x" });
            let error = env_file_body(&Agent::from_request(&request).unwrap()).unwrap_err();
            assert!(error.contains("reserved"), "{key}: {error}");
        }
        let mut request = request();
        request["agent"]["env_vars"] = serde_json::json!({ "BAD KEY": "x" });
        assert!(env_file_body(&Agent::from_request(&request).unwrap())
            .unwrap_err()
            .contains("not a valid identifier"));
    }

    #[test]
    fn env_values_cannot_forge_an_extra_assignment() {
        // A newline would end the assignment and start a line of the value's
        // own choosing — including a line that re-sets a reserved key.
        let error = env_line("X", "a\nBUZZ_PRIVATE_KEY=nsec1evil").unwrap_err();
        assert!(error.contains("control character"));
        // Quotes and backslashes are escaped rather than refused.
        assert_eq!(env_line("X", r#"a"b\c"#).unwrap(), "X=\"a\\\"b\\\\c\"\n");
    }

    #[test]
    fn allowlist_mode_requires_an_allowlist() {
        let mut request = request();
        request["agent"]["respond_to"] = serde_json::json!("allowlist");
        assert!(env_file_body(&Agent::from_request(&request).unwrap()).is_err());

        request["agent"]["respond_to_allowlist"] = serde_json::json!(["abc123"]);
        let body = env_file_body(&Agent::from_request(&request).unwrap()).unwrap();
        assert!(body.contains(r#"BUZZ_ACP_RESPOND_TO_ALLOWLIST="abc123""#));
    }

    #[test]
    fn slugs_are_unit_safe_and_stable_across_redeploys() {
        let agent = Agent::from_request(&request()).unwrap();
        let slug = agent.slug();
        assert!(
            slug.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{slug}"
        );
        assert!(slug.starts_with("research-bot-"));
        // Redeploy is the start path: the same name must yield the same unit
        // and the same agent_id, or start would provision a duplicate.
        assert_eq!(slug, Agent::from_request(&request()).unwrap().slug());
        assert_eq!(agent.agent_id(), format!("buzz-acp@{slug}"));
    }

    #[test]
    fn names_that_sanitize_alike_still_get_distinct_units() {
        let named = |name: &str| {
            let mut request = request();
            request["agent"]["name"] = serde_json::json!(name);
            Agent::from_request(&request).unwrap().slug()
        };
        assert_ne!(named("Bot!"), named("Bot?"));
        // A name with nothing usable still produces a legal instance name.
        let empty = named("!!!");
        assert!(empty.starts_with("agent-"), "{empty}");
    }

    #[test]
    fn redeploy_is_idempotent() {
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        // One templated unit per host, reloaded only when its content changed.
        assert!(script.contains(r#"unit_file="$units/buzz-acp@.service""#));
        assert!(script.contains(r#"if cmp -s "$unit_file.new" "$unit_file""#));
        assert_eq!(script.matches("daemon-reload").count(), 1);
        // Enable is idempotent; restart makes an already-running unit adopt the
        // rewritten env file.
        assert!(script.contains("systemctl --user enable --now 'buzz-acp@research-bot-"));
        assert!(script.contains("systemctl --user restart 'buzz-acp@research-bot-"));
        // The env file is replaced atomically, so a failed write never leaves a
        // half-written identity behind.
        assert!(script.contains(r#"mv "$tmp" "$env_file""#));
    }

    #[test]
    fn the_unit_template_substitutes_a_resolved_buzz_acp() {
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        assert!(UNIT_TEMPLATE.contains("ExecStart=@BUZZ_ACP_BIN@"));
        assert!(UNIT_TEMPLATE.contains("EnvironmentFile=%h/.config/buzz-acp/%i.env"));
        // Substitution is parameter expansion, not `sed`: `sed -i` is a GNU
        // extension that BSD and macOS hosts reject.
        assert!(!script.contains("sed "));
        assert!(script.contains("${template%%@BUZZ_ACP_BIN@*}$acp${template#*@BUZZ_ACP_BIN@}"));
        // Lingering, or the agent dies when this SSH session ends. It also
        // creates /run/user/$(id -u), so it must precede any bus traffic.
        let linger = script.find("loginctl enable-linger").unwrap();
        assert!(linger < script.find("systemctl --user").unwrap());
        // A non-interactive SSH command often has no XDG_RUNTIME_DIR, and
        // without it every `systemctl --user` fails to reach the bus.
        assert!(script.contains(r#"if [ -z "${XDG_RUNTIME_DIR:-}" ]; then"#));
    }

    /// Run the generated script against a real `/bin/sh` in a sandbox, with
    /// `systemctl`/`loginctl` stubbed out.
    ///
    /// Substring assertions prove the script *says* the right things; only
    /// executing it proves it *is* a valid shell program that produces the
    /// right files. Everything below — quoting, heredoc framing, the
    /// `@BUZZ_ACP_BIN@` expansion, `set -eu` interactions — is the kind of
    /// defect no `contains` check catches.
    fn run_deploy_script(
        sandbox: &str,
        request: &serde_json::Value,
    ) -> (std::process::Output, std::path::PathBuf) {
        let root = sandbox_host(sandbox, HostAcp::Installed);
        let agent = Agent::from_request(request).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        (run_in_sandbox(&root, &script), root)
    }

    /// Whether the sandboxed "host" already has `buzz-acp` on its PATH. The
    /// install path only engages on a host that does not.
    #[derive(PartialEq)]
    enum HostAcp {
        Installed,
        Missing,
    }

    /// Build the fake host: a `$HOME` with a stubbed `bin` on its PATH.
    fn sandbox_host(sandbox: &str, acp: HostAcp) -> std::path::PathBuf {
        // Named per test rather than keyed on the thread id, which the test
        // harness recycles once a thread finishes.
        let root =
            std::env::temp_dir().join(format!("buzz-deploy-{}-{sandbox}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Stub every host binary the script resolves, so the run is hermetic.
        let mut stubs = vec![
            ("goose", "#!/bin/sh\nexit 0\n"),
            ("git-credential-nostr", "#!/bin/sh\nexit 0\n"),
            // Record the systemd calls instead of making them.
            (
                "systemctl",
                "#!/bin/sh\nprintf 'systemctl %s\\n' \"$*\" >> \"$HOME/systemd.log\"\n",
            ),
            (
                "loginctl",
                "#!/bin/sh\nprintf 'loginctl %s\\n' \"$*\" >> \"$HOME/systemd.log\"\n",
            ),
        ];
        if acp == HostAcp::Installed {
            stubs.push(("buzz-acp", "#!/bin/sh\nexit 0\n"));
        }
        for (name, body) in stubs {
            let path = bin.join(name);
            std::fs::write(&path, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        root
    }

    /// Feed `script` to a real `/bin/sh` exactly as `ssh` feeds it to the
    /// remote one: on stdin, with the sandbox as `$HOME`.
    fn run_in_sandbox(root: &std::path::Path, script: &str) -> std::process::Output {
        let bin = root.join("bin");
        std::process::Command::new("/bin/sh")
            .arg("-s")
            .env_clear()
            .env("HOME", root)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(script.as_bytes())
                    .unwrap();
                child.wait_with_output()
            })
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn the_generated_script_actually_runs_and_provisions_the_host() {
        let (output, root) = run_deploy_script("provision", &request());
        assert!(
            output.status.success(),
            "deploy script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let slug = Agent::from_request(&request()).unwrap().slug();
        let env_file = root.join(".config/buzz-acp").join(format!("{slug}.env"));
        let written = std::fs::read_to_string(&env_file).unwrap();

        // The harness pin, resolved to an absolute path on the host, and
        // written where systemd re-reads it on every restart.
        assert!(
            written.contains(&format!(
                "BUZZ_ACP_AGENT_COMMAND=\"{}\"",
                root.join("bin/goose").display()
            )),
            "{written}"
        );
        assert!(written.contains(&format!("BUZZ_PRIVATE_KEY=\"{NSEC}\"")));
        assert!(written.contains("ANTHROPIC_API_KEY=\"sk-ant-secret\""));
        // The git block only lands because the stub helper exists, and it
        // carries the helper's resolved path.
        assert!(written.contains(&format!(
            "GIT_CONFIG_VALUE_0=\"{}\"",
            root.join("bin/git-credential-nostr").display()
        )));
        assert!(written.contains(&format!("NOSTR_PRIVATE_KEY=\"{NSEC}\"")));
        // Every line is a well-formed assignment: no heredoc marker leaked in,
        // and no value split across lines.
        for line in written.lines() {
            assert!(
                line.split_once('=')
                    .is_some_and(|(_, v)| v.starts_with('"') && v.ends_with('"') && v.len() >= 2),
                "malformed env line: {line}"
            );
        }

        // Only the owner can read the minted key.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&env_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "env file is group/world accessible");
        }

        // The unit landed with a real path in ExecStart, and no placeholder.
        let unit =
            std::fs::read_to_string(root.join(".config/systemd/user/buzz-acp@.service")).unwrap();
        assert!(unit.contains(&format!(
            "ExecStart={}",
            root.join("bin/buzz-acp").display()
        )));
        assert!(!unit.contains("@BUZZ_ACP_BIN@"));
        assert!(!root
            .join(".config/systemd/user/buzz-acp@.service.new")
            .exists());

        let calls = std::fs::read_to_string(root.join("systemd.log")).unwrap();
        assert!(calls.contains("loginctl enable-linger"));
        assert!(calls.contains("systemctl --user daemon-reload"));
        assert!(calls.contains(&format!(
            "systemctl --user enable --now buzz-acp@{slug}.service"
        )));
        assert!(calls.contains(&format!("systemctl --user restart buzz-acp@{slug}.service")));
    }

    #[cfg(unix)]
    #[test]
    fn a_second_deploy_reuses_the_unit_and_skips_the_reload() {
        let (first, root) = run_deploy_script("redeploy", &request());
        assert!(first.status.success());
        std::fs::remove_file(root.join("systemd.log")).unwrap();

        // Redeploy is the start path, so this is what `start_managed_agent`
        // does on every start. The unit content is unchanged, so systemd must
        // not be reloaded — but the env file must still be rewritten and the
        // service restarted onto it.
        let (second, _) = run_deploy_script("redeploy", &request());
        assert!(
            second.status.success(),
            "redeploy failed: {}",
            String::from_utf8_lossy(&second.stderr)
        );
        let calls = std::fs::read_to_string(root.join("systemd.log")).unwrap();
        assert!(!calls.contains("daemon-reload"), "{calls}");
        assert!(calls.contains("restart"), "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_harness_stops_the_deploy_before_anything_is_written() {
        let mut request = request();
        request["agent"]["agent_command"] = serde_json::json!("not-installed-anywhere");
        let (output, root) = run_deploy_script("missing-harness", &request);
        assert_eq!(output.status.code(), Some(91));
        assert!(String::from_utf8_lossy(&output.stderr).contains("not-installed-anywhere"));
        // `set -e` plus ordering: nothing is provisioned on a failed resolve.
        assert!(!root.join(".config/buzz-acp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn shell_metacharacters_in_the_payload_stay_inert() {
        // Regression: `quote()` makes a value inert as an *argument*, but
        // interpolating that quoted form into a double-quoted string (an error
        // message, say) leaves its single quotes literal and lets a `$(...)`
        // in the payload execute. Every field below is attacker-influenced, so
        // this test runs the script for real and checks that none of the
        // command substitutions fired.
        let canary = std::env::temp_dir().join(format!("buzz-pwned-{}", std::process::id()));
        let _ = std::fs::remove_file(&canary);
        let payload = format!("$(touch {})", canary.display());

        let mut request = request();
        request["agent"]["name"] = serde_json::json!(format!("bot {payload}"));
        request["agent"]["agent_command"] = serde_json::json!(payload);
        request["agent"]["relay_url"] = serde_json::json!(format!("wss://relay/{payload}"));
        request["agent"]["model"] = serde_json::json!(payload.clone());
        request["agent"]["env_vars"] = serde_json::json!({ "EVIL": payload.clone() });

        let (output, _) = run_deploy_script("injection", &request);
        // The harness does not exist, so the deploy stops — the point is that
        // it stops without having executed the payload.
        assert_eq!(output.status.code(), Some(91));
        assert!(
            !canary.exists(),
            "payload executed on the host: command injection in the deploy script"
        );

        // And the slug stays a legal systemd instance name regardless.
        let agent = Agent::from_request(&request).unwrap();
        assert!(agent
            .slug()
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    }

    /// A "buzz-acp" to push: a legal ELF header followed by every byte
    /// sequence that would end the heredoc, escape the script, or run a command
    /// if the transport were anything other than base64.
    fn canary_binary(canary: &std::path::Path) -> Vec<u8> {
        let mut bytes = b"\x7fELF\x02\x01\x01\x00".to_vec();
        bytes.extend_from_slice(format!("$(touch {})\n", canary.display()).as_bytes());
        bytes.extend_from_slice(format!("`touch {}`\n", canary.display()).as_bytes());
        bytes.extend_from_slice(b"BUZZ_ACP_B64_EOF\nrm -rf \"$HOME\"\n");
        bytes.extend_from_slice(b"\0'\"\r\n$HOME ${HOME}\n");
        bytes.extend_from_slice(&(0u8..=255).collect::<Vec<u8>>());
        bytes
    }

    fn push_payload(name: &str, bytes: &[u8]) -> Payload {
        let path =
            std::env::temp_dir().join(format!("buzz-acp-push-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        Payload::read(&path.display().to_string()).unwrap()
    }

    #[test]
    fn the_pushed_binary_is_an_optional_field_that_changes_nothing_when_absent() {
        // The seam must be invisible: a payload without the field produces the
        // script the crate produced before the field existed.
        let mut request = request();
        let agent = Agent::from_request(&request).unwrap();
        assert!(agent.buzz_acp_binary.is_none());
        let without = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        assert!(without.contains("exit 90"));
        assert!(!without.contains("base64 -d"));

        // A blank string is "absent", not "push nothing".
        request["agent"]["buzz_acp_binary"] = serde_json::json!("   ");
        assert!(Agent::from_request(&request)
            .unwrap()
            .buzz_acp_binary
            .is_none());

        request["agent"]["buzz_acp_binary"] = serde_json::json!("/opt/buzz-acp");
        assert_eq!(
            Agent::from_request(&request).unwrap().buzz_acp_binary,
            Some("/opt/buzz-acp".to_string())
        );
    }

    #[test]
    fn a_pushed_binary_never_displaces_the_secret_discipline() {
        let canary = std::env::temp_dir().join("buzz-never");
        let payload = push_payload("discipline", &canary_binary(&canary));
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, Some(&payload)).unwrap();

        // The install block is additive: everything the secret path relies on
        // is still exactly where it was.
        assert!(script.starts_with("set -eu\numask 077\n"));
        assert!(script.contains("chmod 600 \"$tmp\""));
        assert!(script.contains(&format!("BUZZ_PRIVATE_KEY=\"{NSEC}\"")));
        // The binary is resolved/installed BEFORE the unit is templated, so
        // `$acp` — and therefore ExecStart — names the copy just installed.
        let install = script.find("base64 -d").unwrap();
        assert!(install < script.find("unit_file=").unwrap());
        // The hash travels in the clear (it is a fingerprint, not a secret) and
        // the encoded bytes carry nothing the shell reads as syntax.
        assert!(script.contains(payload.sha256()));
    }

    #[cfg(unix)]
    #[test]
    fn a_pushed_binary_installs_atomically_and_only_after_it_verifies() {
        let canary = std::env::temp_dir().join(format!("buzz-push-pwned-{}", std::process::id()));
        let _ = std::fs::remove_file(&canary);
        let bytes = canary_binary(&canary);
        let payload = push_payload("install", &bytes);

        // A host with no `buzz-acp` at all — the only case the push engages.
        let root = sandbox_host("install", HostAcp::Missing);
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, Some(&payload)).unwrap();
        let output = run_in_sandbox(&root, &script);
        assert!(
            output.status.success(),
            "install deploy failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Byte-identical after a round trip through base64, a heredoc, and a
        // real `/bin/sh` — including the NULs, quotes, `$(...)` and the literal
        // heredoc delimiter embedded in the payload.
        let installed = root.join(".local/bin/buzz-acp");
        assert_eq!(std::fs::read(&installed).unwrap(), bytes);
        assert!(
            !canary.exists(),
            "the pushed binary's contents executed on the host"
        );

        // Executable, and no temp file left behind.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "installed binary is not 755");
        assert!(!leftover_temp_files(&root.join(".local/bin")));

        // And the unit points at the copy this pass installed, in the same
        // deploy — install first, resolve second.
        let unit =
            std::fs::read_to_string(root.join(".config/systemd/user/buzz-acp@.service")).unwrap();
        assert!(
            unit.contains(&format!("ExecStart={}", installed.display())),
            "{unit}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_corrupted_push_aborts_before_the_mv_and_leaves_nothing_runnable() {
        let canary = std::env::temp_dir().join("buzz-never");
        let payload = push_payload("mismatch", &canary_binary(&canary));
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, Some(&payload)).unwrap();
        // Stand in for a payload damaged in flight: the host is told to expect
        // a digest the decoded bytes cannot produce.
        let script = script.replace(payload.sha256(), &"a".repeat(64));

        let root = sandbox_host("mismatch", HostAcp::Missing);
        let output = run_in_sandbox(&root, &script);
        assert_eq!(output.status.code(), Some(94));
        assert!(String::from_utf8_lossy(&output.stderr).contains("sha256"));

        // Nothing installed, and — the property that matters — no half-written
        // executable left in the directory systemd's ExecStart would name.
        assert!(!root.join(".local/bin/buzz-acp").exists());
        assert!(!leftover_temp_files(&root.join(".local/bin")));
        // The deploy stopped there: no env file, no unit.
        assert!(!root.join(".config/buzz-acp").exists());
        assert!(!root.join(".config/systemd").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_host_without_buzz_acp_and_no_pushed_binary_still_fails_with_todays_guidance() {
        // The un-pushed path is unchanged: exit 90 and the same message, so a
        // user who never sets the seam sees exactly what they saw before.
        let root = sandbox_host("no-acp", HostAcp::Missing);
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        let output = run_in_sandbox(&root, &script);
        assert_eq!(output.status.code(), Some(90));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("buzz-acp not found on the server's PATH"),
            "{stderr}"
        );
        assert!(!root.join(".local/bin/buzz-acp").exists());
        assert!(!root.join(".config/buzz-acp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_host_binary_is_never_replaced_by_the_pushed_one() {
        // Staleness rule: push-when-missing only. Deploy is the start path, so
        // a version-comparing rule would reinstall underneath a running fleet
        // on every start — and a desktop pinned to an older artifact would
        // downgrade the host.
        let canary = std::env::temp_dir().join("buzz-never");
        let payload = push_payload("keep", &canary_binary(&canary));
        let root = sandbox_host("keep", HostAcp::Installed);
        let existing = std::fs::read(root.join("bin/buzz-acp")).unwrap();

        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, Some(&payload)).unwrap();
        let output = run_in_sandbox(&root, &script);
        assert!(
            output.status.success(),
            "deploy failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(std::fs::read(root.join("bin/buzz-acp")).unwrap(), existing);
        assert!(
            !root.join(".local/bin/buzz-acp").exists(),
            "a host that already had buzz-acp got a second copy installed"
        );
        let unit =
            std::fs::read_to_string(root.join(".config/systemd/user/buzz-acp@.service")).unwrap();
        assert!(unit.contains(&format!(
            "ExecStart={}",
            root.join("bin/buzz-acp").display()
        )));
    }

    #[cfg(unix)]
    #[test]
    fn a_second_deploy_keeps_the_binary_the_first_one_installed() {
        // The install destination — `~/.local/bin` — is NOT on a
        // non-interactive SSH PATH, which is exactly why the env file below
        // pins `PATH="$HOME/.local/bin:$PATH"` itself. Resolution has to say so
        // too: with a bare `command -v`, the probe answered "missing" forever
        // and every deploy re-streamed and replaced the binary. Deploy is the
        // start path, so that is every agent start, underneath a running fleet.
        //
        // `an_existing_host_binary_is_never_replaced_by_the_pushed_one` cannot
        // see this: it seeds the stub into the sandbox's `bin`, which IS on the
        // sandbox PATH.
        use std::os::unix::fs::MetadataExt;

        let canary = std::env::temp_dir().join("buzz-never");
        let payload = push_payload("twice", &canary_binary(&canary));
        let root = sandbox_host("twice", HostAcp::Missing);
        let agent = Agent::from_request(&request()).unwrap();
        let installed = root.join(".local/bin/buzz-acp");

        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, Some(&payload)).unwrap();
        let first = run_in_sandbox(&root, &script);
        assert!(
            first.status.success(),
            "first deploy failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        let inode = std::fs::metadata(&installed).unwrap().ino();

        // What the desktop asks before deploy #2. It must now answer "the host
        // has it", which is what keeps the payload off the wire — the file is
        // not even read, let alone encoded and streamed.
        let acp = quote(config().buzz_acp_path.as_deref().unwrap_or("buzz-acp"));
        let probe = run_in_sandbox(&root, &install::probe_script(&acp));
        assert!(
            probe.status.success(),
            "the probe did not see the binary the previous deploy installed"
        );

        // And even the worst case — a script that still carries the payload —
        // resolves to the installed copy instead of replacing it.
        let second = run_in_sandbox(&root, &script);
        assert!(
            second.status.success(),
            "second deploy failed: {}",
            String::from_utf8_lossy(&second.stderr)
        );
        assert_eq!(
            std::fs::metadata(&installed).unwrap().ino(),
            inode,
            "the second deploy replaced the binary the first one installed"
        );

        // The unit still names it, so idempotence is real and not just quiet.
        let unit =
            std::fs::read_to_string(root.join(".config/systemd/user/buzz-acp@.service")).unwrap();
        assert!(
            unit.contains(&format!("ExecStart={}", installed.display())),
            "{unit}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_absolute_path_still_resolves_the_copy_deploy_installed() {
        // `buzz_acp_path` is an absolute path the operator picked, but an
        // install always lands in `~/.local/bin`. Resolving only what was
        // configured would never find it, so the host would re-install on every
        // single start, forever.
        use std::os::unix::fs::MetadataExt;

        let canary = std::env::temp_dir().join("buzz-never");
        let payload = push_payload("configured", &canary_binary(&canary));
        let root = sandbox_host("configured", HostAcp::Missing);
        let config = SshConfig {
            buzz_acp_path: Some("/opt/buzz-acp".into()),
            ..config()
        };
        let agent = Agent::from_request(&request()).unwrap();
        let installed = root.join(".local/bin/buzz-acp");

        let script = deploy_script(&agent, &config, UNIT_TEMPLATE, Some(&payload)).unwrap();
        assert!(run_in_sandbox(&root, &script).status.success());
        let inode = std::fs::metadata(&installed).unwrap().ino();

        let acp = quote(config.buzz_acp_path.as_deref().unwrap_or("buzz-acp"));
        assert!(
            run_in_sandbox(&root, &install::probe_script(&acp))
                .status
                .success(),
            "the probe missed the install because the configured path is elsewhere"
        );
        assert!(run_in_sandbox(&root, &script).status.success());
        assert_eq!(std::fs::metadata(&installed).unwrap().ino(), inode);
    }

    #[cfg(unix)]
    #[test]
    fn a_payload_that_decodes_to_garbage_aborts_before_anything_is_installed() {
        let canary = std::env::temp_dir().join("buzz-never");
        let payload = push_payload("decode", &canary_binary(&canary));
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, Some(&payload)).unwrap();
        // Stand in for a stream truncated in flight. `!` is outside the base64
        // alphabet, so `base64 -d` rejects the body outright — the exit-93
        // branch, which the sha256 test cannot reach because a payload that
        // fails to decode never gets as far as being hashed.
        let corrupt = script.replacen(&payload.encoded()[..8], "!!!!!!!!", 1);
        assert_ne!(corrupt, script, "the encoded body was not corrupted");

        let root = sandbox_host("decode", HostAcp::Missing);
        let output = run_in_sandbox(&root, &corrupt);
        assert_eq!(output.status.code(), Some(93));
        assert!(String::from_utf8_lossy(&output.stderr).contains("decode"));
        assert!(!root.join(".local/bin/buzz-acp").exists());
        assert!(!leftover_temp_files(&root.join(".local/bin")));
        // The `|| { ... }` really does bind to the heredoc-fed command: the
        // deploy stopped here rather than running on with a corrupt file.
        assert!(!root.join(".config/systemd").exists());
    }

    /// Any `.buzz-acp.tmp.*` still sitting in `dir`. A half-written binary that
    /// survives a failed install is the failure mode the temp-file dance exists
    /// to prevent.
    fn leftover_temp_files(dir: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".buzz-acp.tmp.")
        })
    }

    #[test]
    fn relay_urls_map_to_their_http_origin_for_git_auth() {
        assert_eq!(
            relay_http_base_url("wss://relay.example/"),
            "https://relay.example"
        );
        assert_eq!(
            relay_http_base_url("ws://localhost:8080"),
            "http://localhost:8080"
        );
        assert_eq!(
            relay_http_base_url("https://relay.example"),
            "https://relay.example"
        );
    }

    #[test]
    fn git_credential_env_is_emitted_only_when_the_helper_exists() {
        let agent = Agent::from_request(&request()).unwrap();
        let script = deploy_script(&agent, &config(), UNIT_TEMPLATE, None).unwrap();
        assert!(script.contains(r#"cred=$(command -v git-credential-nostr 2>/dev/null || true)"#));
        assert!(script.contains(r#"if [ -n "$cred" ]; then"#));
        assert!(
            script.contains(r#"GIT_CONFIG_KEY_0="credential.https://relay.example/ws/git.helper""#)
        );
    }
}
