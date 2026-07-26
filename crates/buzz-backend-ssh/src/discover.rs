//! `check`, `discover_harnesses` and `probe_models`: everything that reads the
//! remote host without changing it.
//!
//! All three are **one SSH round trip**. `discover_harnesses` in particular
//! probes every candidate harness from a single generated script — N sequential
//! `ssh` invocations would spend the whole 45s budget on handshakes over a
//! 200 ms link, and the harness picker would visibly hang.

use std::time::Duration;

use crate::protocol::{snippet, SshConfig};
use crate::ssh::{quote, Session};

/// Harnesses the desktop knows how to render, in the same vocabulary its local
/// catalog uses (`KNOWN_ACP_RUNTIMES` + `PRESET_HARNESSES` in
/// `managed_agents/discovery.rs`). Ids must satisfy `[a-z0-9_][a-z0-9_-]*` or
/// `validate_harness_definition` drops the entry desktop-side.
struct Candidate {
    id: &'static str,
    label: &'static str,
    /// Accepted command names, most preferred first.
    commands: &'static [&'static str],
    args: &'static [&'static str],
    /// The runtime's `default_env` (`discovery.rs`). Locally these are applied
    /// at spawn time from the catalog; a remote agent never spawns locally, so
    /// they must ride along in the `HarnessDefinition` the desktop pins, or
    /// they are simply lost.
    env: &'static [(&'static str, &'static str)],
}

const CANDIDATES: &[Candidate] = &[
    Candidate {
        id: "buzz-agent",
        label: "Buzz Agent",
        commands: &["buzz-agent"],
        args: &[],
        env: &[],
    },
    Candidate {
        id: "goose",
        label: "Goose",
        commands: &["goose"],
        args: &["acp"],
        // Without this a remote Goose blocks on tool approvals that nobody is
        // present to answer, and the agent silently stops making progress.
        env: &[("GOOSE_MODE", "auto")],
    },
    Candidate {
        id: "claude",
        label: "Claude Code",
        commands: &["claude-agent-acp", "claude-code-acp"],
        args: &[],
        env: &[],
    },
    Candidate {
        id: "codex",
        label: "Codex",
        commands: &["codex-acp"],
        args: &[],
        env: &[],
    },
    Candidate {
        id: "cursor",
        label: "Cursor",
        commands: &["cursor-agent"],
        args: &["acp"],
        env: &[],
    },
    Candidate {
        id: "omp",
        label: "Oh My Pi",
        commands: &["omp"],
        args: &["acp"],
        env: &[],
    },
    Candidate {
        id: "grok",
        label: "Grok Build",
        commands: &["grok"],
        args: &["agent", "--always-approve", "stdio"],
        env: &[],
    },
    Candidate {
        id: "opencode",
        label: "OpenCode",
        commands: &["opencode"],
        args: &["acp"],
        env: &[],
    },
    Candidate {
        id: "kimi",
        label: "Kimi Code",
        commands: &["kimi"],
        args: &["acp"],
        env: &[],
    },
    Candidate {
        id: "amp",
        label: "Amp",
        commands: &["amp-acp"],
        args: &[],
        env: &[],
    },
    Candidate {
        id: "hermes",
        label: "Hermes Agent",
        commands: &["hermes-acp"],
        args: &[],
        env: &[],
    },
    Candidate {
        id: "openclaw",
        label: "OpenClaw",
        commands: &["openclaw"],
        args: &["acp"],
        env: &[],
    },
];

/// Preamble shared by every remote script.
///
/// `probe` writes one tab-separated record per resolved command, rather than
/// JSON assembled in `sh`: quoting arbitrary `--version` output into valid JSON
/// from a POSIX shell is a bug farm, and the parsing belongs on the Rust side
/// where it is testable.
///
/// Two details are load-bearing. `</dev/null` on every probed child, because
/// the script itself arrives on the remote shell's stdin and a child that reads
/// stdin would swallow the rest of it. And `timeout` where the host has it, so
/// a harness whose `--version` opens a REPL cannot hold the budget.
const PROBE_PREAMBLE: &str = r#"set -u
if command -v timeout >/dev/null 2>&1; then _t="timeout 5"; else _t=""; fi
probe() {
  _p=$(command -v "$2" 2>/dev/null) || return 0
  [ -n "$_p" ] || return 0
  _v=$($_t "$2" --version </dev/null 2>/dev/null | head -n 1 | tr -d '\t\r') || _v=""
  printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$_p" "$_v"
}
"#;

/// The one probe script: `buzz-acp` plus every harness candidate.
fn discover_script(config: &SshConfig) -> String {
    let mut script = String::from(PROBE_PREAMBLE);
    let acp = config.buzz_acp_path.as_deref().unwrap_or("buzz-acp");
    script.push_str(&format!("probe 'buzz-acp' {}\n", quote(acp)));
    for candidate in CANDIDATES {
        for command in candidate.commands {
            script.push_str(&format!(
                "probe {} {}\n",
                quote(candidate.id),
                quote(command)
            ));
        }
    }
    script
}

/// One `probe` record: `key<TAB>command<TAB>path<TAB>version`.
struct Probe<'a> {
    key: &'a str,
    command: &'a str,
    path: &'a str,
    version: &'a str,
}

fn parse_probes(stdout: &str) -> Vec<Probe<'_>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            let probe = Probe {
                key: fields.next()?,
                command: fields.next()?,
                path: fields.next()?,
                version: fields.next().unwrap_or(""),
            };
            (!probe.key.is_empty() && !probe.path.is_empty()).then_some(probe)
        })
        .collect()
}

/// Shape the probe records into the `discover_harnesses` response.
///
/// Every element is a `HarnessDefinition` (camelCase, exactly the desktop's
/// own wire type) plus `available` / `binaryPath` / `version`. Unresolved
/// candidates are still reported, with `available: false`, so the picker can
/// say "install this on the host" instead of hiding the option.
fn harnesses_response(stdout: &str) -> serde_json::Value {
    let probes = parse_probes(stdout);
    let buzz_acp = probes
        .iter()
        .find(|probe| probe.key == "buzz-acp")
        .map(|probe| serde_json::json!({ "path": probe.path, "version": probe.version }))
        .unwrap_or(serde_json::Value::Null);

    let harnesses: Vec<serde_json::Value> = CANDIDATES
        .iter()
        .map(|candidate| {
            let found = probes.iter().find(|probe| probe.key == candidate.id);
            serde_json::json!({
                "id": candidate.id,
                "label": candidate.label,
                // The remote command name. This is what the desktop pins as the
                // create-time harness override, so it must name a binary on the
                // HOST, never one resolved locally.
                "command": found.map_or(candidate.commands[0], |probe| probe.command),
                "args": candidate.args,
                "env": candidate
                    .env
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), serde_json::Value::from(*value)))
                    .collect::<serde_json::Map<_, _>>(),
                "installInstructionsUrl": "",
                "installHint": "",
                "available": found.is_some(),
                "binaryPath": found.map(|probe| probe.path),
                "version": found.map(|probe| probe.version).filter(|v| !v.is_empty()),
            })
        })
        .collect();

    // `buzz_acp: null` with `ok: true` is deliberate: the UI can then render an
    // actionable "install buzz-acp on this host" instead of a bare failure.
    serde_json::json!({ "ok": true, "buzz_acp": buzz_acp, "harnesses": harnesses })
}

pub fn discover_harnesses(
    config: &SshConfig,
    session: &Session,
) -> Result<serde_json::Value, String> {
    let output = session.run(&discover_script(config), Duration::from_secs(40))?;
    if !output.ok() {
        return Err(output.failure());
    }
    Ok(harnesses_response(&output.stdout))
}

/// `check`: the preflight the create dialog runs before Deploy goes live.
pub fn check(session: &Session) -> Result<serde_json::Value, String> {
    let output = session.run("echo buzz-ok\n", Duration::from_secs(8))?;
    if output.stdout.trim() == "buzz-ok" {
        return Ok(serde_json::json!({ "ok": true, "detail": "Connected" }));
    }
    Err(guidance(&output.failure()))
}

/// Turn ssh's own diagnosis into something the user can act on. The classified
/// causes are the ones that actually happen; everything else passes through
/// verbatim rather than being flattened into a generic message.
fn guidance(failure: &str) -> String {
    const GUIDANCE: &[(&str, &str)] = &[
        ("permission denied", "add your public key to ~/.ssh/authorized_keys on the server, or run `tailscale set --ssh` there."),
        ("host key verification failed", "the server's host key is not in your known_hosts. Connect once with `ssh` to review and accept it."),
        ("could not resolve hostname", "check the address, or confirm the device is on your tailnet."),
        ("connection refused", "confirm the server is reachable and running an SSH daemon."),
        ("connection timed out", "confirm the server is reachable and running an SSH daemon."),
    ];

    let lower = failure.to_lowercase();
    match GUIDANCE.iter().find(|(cause, _)| lower.contains(cause)) {
        Some((_, advice)) => format!("{failure} — {advice}"),
        None => failure.to_string(),
    }
}

/// `probe_models`: run `buzz-acp models --json` on the host and hand the raw
/// document back untouched.
///
/// Verbatim is the point: the desktop feeds it straight into the same
/// `normalize_agent_models` the local path uses, so the model picker needs no
/// remote-specific code at all.
pub fn probe_models(
    request: &serde_json::Value,
    config: &SshConfig,
    session: &Session,
) -> Result<serde_json::Value, String> {
    let output = session.run(&models_script(request, config)?, Duration::from_secs(110))?;
    if !output.ok() {
        return Err(output.failure());
    }
    let models_raw: serde_json::Value =
        serde_json::from_str(output.stdout.trim()).map_err(|e| {
            format!(
                "`buzz-acp models --json` did not return JSON ({e}): {}",
                snippet(&output.stdout)
            )
        })?;
    Ok(serde_json::json!({ "ok": true, "models_raw": models_raw }))
}

fn models_script(request: &serde_json::Value, config: &SshConfig) -> Result<String, String> {
    let harness = request
        .get("harness")
        .ok_or("probe_models request is missing 'harness'")?;
    let command = harness
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or("probe_models harness is missing 'command'")?;
    let args = string_list(harness.get("args"));

    // `agent.env_vars` is the only place `env_secrets_from_request`
    // (backend.rs) looks when scrubbing these values out of an error surface,
    // so a top-level `model_env` would travel unredacted through any failure
    // message. Still accepted, since the transport is safe either way; it just
    // loses that second layer.
    let model_env = request
        .get("agent")
        .and_then(|agent| agent.get("env_vars"))
        .or_else(|| request.get("model_env"));

    let mut script = String::from("set -u\n");
    // Model-probe env carries provider API keys, set inside the
    // stdin-delivered script so they never appear in the remote argv.
    //
    // Names are validated rather than quoted: on the left of an assignment
    // quoting has no effect, so an unchecked name is a straight command
    // injection (`X=1; touch /tmp/pwn`). Quoting is sufficient for values.
    for (key, value) in crate::deploy::env_map(model_env) {
        if !crate::deploy::is_well_formed_env_key(&key) {
            return Err(format!("env var name '{key}' is not a valid identifier"));
        }
        script.push_str(&format!("export {}={}\n", key, quote(&value)));
    }
    script.push_str(&format!(
        "export BUZZ_ACP_AGENT_COMMAND={}\nexport BUZZ_ACP_AGENT_ARGS={}\n",
        quote(command),
        quote(&args.join(","))
    ));
    script.push_str(&format!(
        "exec {} models --json </dev/null\n",
        quote(config.buzz_acp_path.as_deref().unwrap_or("buzz-acp"))
    ));
    Ok(script)
}

pub fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SshConfig {
        SshConfig {
            host: "vps".into(),
            ..SshConfig::default()
        }
    }

    #[test]
    fn discover_is_one_script_covering_every_candidate() {
        let script = discover_script(&config());
        assert!(script.contains("probe 'buzz-acp' 'buzz-acp'"));
        for candidate in CANDIDATES {
            for command in candidate.commands {
                assert!(
                    script.contains(&format!("probe '{}' '{command}'", candidate.id)),
                    "missing probe for {command}"
                );
            }
        }
        // Children must not read the script off the shell's own stdin.
        assert!(script.contains("</dev/null"));
    }

    #[test]
    fn candidate_ids_satisfy_the_desktop_harness_id_rule() {
        for candidate in CANDIDATES {
            let mut chars = candidate.id.chars();
            let first = chars.next().unwrap();
            assert!(
                first.is_ascii_lowercase() || first.is_ascii_digit() || first == '_',
                "{} has an illegal first character",
                candidate.id
            );
            assert!(
                candidate
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
                "{} has illegal characters",
                candidate.id
            );
            assert!(!candidate.label.trim().is_empty());
            assert!(!candidate.commands.is_empty());
            // `validate_harness_definition` runs every advertised `env` through
            // `validate_user_env_keys`, which rejects reserved and malformed
            // keys — and drops the whole harness if any fail.
            for (key, _) in candidate.env {
                assert!(
                    !key.is_empty()
                        && !key.starts_with(|c: char| c.is_ascii_digit())
                        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "{} advertises a malformed env key {key:?}",
                    candidate.id
                );
                assert!(
                    !key.to_ascii_uppercase().starts_with("BUZZ_"),
                    "{} advertises reserved env key {key:?}",
                    candidate.id
                );
            }
        }
    }

    #[test]
    fn a_configured_buzz_acp_path_is_quoted_into_the_script() {
        let config = SshConfig {
            buzz_acp_path: Some("/opt/buzz acp/bin/buzz-acp".into()),
            ..config()
        };
        assert!(discover_script(&config).contains("'/opt/buzz acp/bin/buzz-acp'"));
    }

    #[test]
    fn probe_records_become_available_harnesses() {
        let stdout = "buzz-acp\tbuzz-acp\t/usr/local/bin/buzz-acp\t0.4.26\n\
                      goose\tgoose\t/home/ubuntu/.local/bin/goose\tgoose 1.9.0\n";
        let response = harnesses_response(stdout);
        assert_eq!(response["ok"], true);
        assert_eq!(response["buzz_acp"]["path"], "/usr/local/bin/buzz-acp");
        assert_eq!(response["buzz_acp"]["version"], "0.4.26");

        let harnesses = response["harnesses"].as_array().unwrap();
        assert_eq!(harnesses.len(), CANDIDATES.len());
        let goose = harnesses.iter().find(|h| h["id"] == "goose").unwrap();
        assert_eq!(goose["available"], true);
        assert_eq!(goose["binaryPath"], "/home/ubuntu/.local/bin/goose");
        assert_eq!(goose["version"], "goose 1.9.0");
        assert_eq!(goose["command"], "goose");
        assert_eq!(goose["args"], serde_json::json!(["acp"]));
        // A HarnessDefinition, in the desktop's own camelCase wire shape.
        // `env` carries the runtime's `default_env`, which local spawn applies
        // from the catalog and a remote deploy can only get from here.
        assert_eq!(goose["env"], serde_json::json!({ "GOOSE_MODE": "auto" }));
        assert!(goose.get("installInstructionsUrl").is_some());

        let absent = harnesses.iter().find(|h| h["id"] == "kimi").unwrap();
        assert_eq!(absent["available"], false);
        assert!(absent["binaryPath"].is_null());
        assert!(absent["version"].is_null());
    }

    #[test]
    fn a_host_without_buzz_acp_still_succeeds() {
        let response = harnesses_response("goose\tgoose\t/usr/bin/goose\t\n");
        assert_eq!(response["ok"], true);
        assert!(response["buzz_acp"].is_null());
    }

    #[test]
    fn the_reported_command_is_the_one_the_host_actually_has() {
        // `claude` resolves through either adapter name; the pin must carry the
        // one that exists on the host, not the first candidate.
        let response =
            harnesses_response("claude\tclaude-code-acp\t/usr/bin/claude-code-acp\t2.1.0\n");
        let claude = response["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == "claude")
            .unwrap();
        assert_eq!(claude["command"], "claude-code-acp");
    }

    #[test]
    fn probe_parsing_ignores_noise_lines() {
        let probes = parse_probes("garbage\n\ngoose\tgoose\t/usr/bin/goose\tv1\nempty\t\t\t\n");
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].command, "goose");
        assert_eq!(probes[0].version, "v1");
    }

    #[test]
    fn guidance_is_actionable_for_the_cases_that_happen() {
        assert!(
            guidance("ssh failed (exit 255): Permission denied (publickey).")
                .contains("authorized_keys")
        );
        assert!(
            guidance("ssh failed (exit 255): Host key verification failed.")
                .contains("known_hosts")
        );
        assert!(
            guidance("ssh failed (exit 255): ssh: Could not resolve hostname vps")
                .contains("tailnet")
        );
        // Unclassified failures survive verbatim rather than being flattened.
        assert_eq!(
            guidance("ssh failed (exit 1): weird"),
            "ssh failed (exit 1): weird"
        );
    }

    #[test]
    fn model_env_is_exported_inside_the_script_never_on_the_argv() {
        // Nested under `agent.env_vars`, the one shape the desktop's
        // `env_secrets_from_request` scrubber knows how to find.
        let request = serde_json::json!({
            "harness": { "command": "goose", "args": ["acp"] },
            "agent": { "env_vars": { "ANTHROPIC_API_KEY": "sk-ant-secret" } },
        });
        let script = models_script(&request, &config()).unwrap();
        assert!(script.contains("export ANTHROPIC_API_KEY='sk-ant-secret'"));
        assert!(script.contains("export BUZZ_ACP_AGENT_COMMAND='goose'"));
        assert!(script.contains("export BUZZ_ACP_AGENT_ARGS='acp'"));
        // The value is only ever in the script body, which travels on stdin —
        // the remote argv is fixed at `sh -s` by `Session`.
        assert!(script.contains("exec 'buzz-acp' models --json </dev/null"));

        // A flat `model_env` is still honored, for a desktop that has not
        // adopted the nested shape yet.
        let flat = serde_json::json!({
            "harness": { "command": "goose" },
            "model_env": { "ANTHROPIC_API_KEY": "sk-ant-secret" },
        });
        assert!(models_script(&flat, &config())
            .unwrap()
            .contains("export ANTHROPIC_API_KEY='sk-ant-secret'"));
    }

    #[test]
    fn probe_models_requires_a_harness_command() {
        let request = serde_json::json!({ "harness": { "args": ["acp"] } });
        assert!(models_script(&request, &config()).is_err());
        assert!(models_script(&serde_json::json!({}), &config()).is_err());
    }

    /// An env *name* is the left side of a shell assignment, where quoting has
    /// no effect. This runs the generated script for real: a substring
    /// assertion would pass against the injectable form too.
    #[cfg(unix)]
    #[test]
    fn a_malformed_model_env_name_cannot_smuggle_a_command() {
        let canary = std::env::temp_dir().join("buzz-models-injection-canary");
        let _ = std::fs::remove_file(&canary);

        let request = serde_json::json!({
            "harness": { "command": "goose" },
            "agent": { "env_vars": {
                format!("X=1; touch {}", canary.display()): "v",
            } },
        });
        let error = models_script(&request, &config())
            .expect_err("a name that is not an identifier must be refused");
        assert!(error.contains("not a valid identifier"), "{error}");

        // Belt and braces: even if the guard were removed, prove the canary
        // path is the one an injection would create.
        assert!(!canary.exists());
    }

    #[test]
    fn every_well_formed_model_env_name_still_passes() {
        let request = serde_json::json!({
            "harness": { "command": "goose" },
            "agent": { "env_vars": { "OPENAI_API_KEY": "sk-1", "_X9": "v" } },
        });
        let script = models_script(&request, &config()).unwrap();
        assert!(script.contains("export OPENAI_API_KEY='sk-1'"));
        assert!(script.contains("export _X9='v'"));
    }
}
