# Remote agents over SSH

A **remote agent** is a managed agent whose harness runs on another host. The desktop still owns
the agent: it mints the agent's nostr key, holds the record, and renders it beside local agents.
It does not own the process. On the host, `buzz-acp` runs as a `systemd --user` unit, and the only
liveness signal the desktop has is the agent's presence on the relay — there is no status op, no
polling channel, and no open connection between deploys.

`buzz-backend-ssh` is the provider binary that puts it there. It is not bundled with the desktop:
`discover_provider_candidates` prepends the app bundle's own directory to the provider search path,
so shipping it inside the bundle would give every install an auto-discovered SSH-deploy capability
and quietly undermine the "only use providers from trusted sources" warning the create dialog
shows. Install it to `~/.local/bin`, which is already on the discovery path.

## Provider protocol

The desktop enumerates PATH (plus the executable's own directory and `~/.local/bin`) for files
named `buzz-backend-<id>`, and resolves `<id>` against `^[a-z0-9][a-z0-9_-]*$`. It spawns the
binary, writes one JSON request to stdin, closes it, and reads one JSON response from stdout. One
process per op; no daemon, no state, no version negotiation.

`buzz-backend-ssh` implements five ops.

| op | opens SSH | provider budget | desktop budget | desktop caller |
|---|---|---|---|---|
| `info` | no | — | 10s | `probe_backend_provider`, to build the host field |
| `check` | yes | 8s | — | none yet |
| `discover_harnesses` | yes | 40s | 60s | `WhereToRunSection`, on "check host" |
| `probe_models` | yes | 110s | 150s | `WhereToRunSection`, after a harness resolves |
| `deploy` | yes | 300s | 600s | `deploy_to_provider`, from create and from start |

The provider budget always fires first, so a timeout arrives as a structured error rather than as a
killed child. Any other `op` value is rejected before a connection is opened, so a typo costs a
parse and not an SSH handshake.

Every desktop-side entry point resolves the provider through `resolve_discovered_provider` before
spawning it, so a frontend or IPC caller that names a `binaryPath` cannot steer execution at an
arbitrary binary.

`info` is the only op that runs before a host is configured — it is what produces the host field —
so it never opens a session and never requires `provider_config`.

```json
{"op": "info", "request_id": "<uuid-v4>"}
```

```json
{"ok": true, "name": "SSH", "version": "…",
 "description": "Run agents on a remote host over SSH, supervised by systemd --user.",
 "config_schema": {"type": "object", "required": ["ssh_host"], "properties": {…}}}
```

`check` is a preflight: `echo buzz-ok` over the configured session. Failures are classified into
actionable guidance (`Permission denied` → `authorized_keys` / `tailscale set --ssh`,
`Host key verification failed` → known_hosts, `Could not resolve hostname` → address or tailnet,
`Connection refused`/`timed out` → reachability). Anything unclassified passes through verbatim
rather than being flattened.

`discover_harnesses` probes `buzz-acp` and every candidate harness in **one** generated `sh`
script. N sequential `ssh` invocations would spend the whole budget on handshakes over a
200 ms link and the harness picker would visibly hang. Two details in that script are load-bearing:
every probed child gets `</dev/null`, because the script itself arrives on the remote shell's stdin
and a child that reads stdin would swallow the rest of it; and each probe runs under `timeout 5`
where the host has it, so a harness whose `--version` opens a REPL cannot hold the budget.

```json
{"op": "discover_harnesses", "request_id": "…",
 "provider_config": {"ssh_host": "vps.example", "ssh_user": "ubuntu"}}
```

```json
{"ok": true,
 "buzz_acp": {"path": "/usr/local/bin/buzz-acp", "version": "0.4.26"},
 "harnesses": [
   {"id": "goose", "label": "Goose", "command": "goose", "args": ["acp"],
    "env": {"GOOSE_MODE": "auto"}, "installInstructionsUrl": "", "installHint": "",
    "available": true, "binaryPath": "/home/ubuntu/.local/bin/goose", "version": "goose 1.9.0"}]}
```

Every element is a `HarnessDefinition` in the desktop's own camelCase wire shape plus
`available`/`binaryPath`/`version`. Unresolved candidates are still reported with
`available: false`, so the picker can say "install this on the host" instead of hiding the option —
but `selectedRemoteHarness` filters the pin on `available`, so an entry a re-check turned
unavailable stops being the pin rather than deploying a command the host says is not installed.
`buzz_acp: null` with `ok: true` is likewise deliberate.

The `command` reported is the one that actually resolved on the host, not the first candidate:
`claude` resolves through `claude-agent-acp` or `claude-code-acp`, and the pin must name the binary
that exists there. `env` carries the runtime's `default_env`, which local spawn applies from the
catalog and a remote deploy can only get from here; the create flow pins it into the record's
`env_vars`, where it lands in the env file underneath any user-set value.

`probe_models` exports the harness env inside the script, then runs `buzz-acp models --json` on the
host and returns the document verbatim under `models_raw`. The desktop feeds it straight into the
same `normalize_agent_models` the local path uses, so the model picker needs no remote-specific
code. Model env must be nested under `agent.env_vars` — that is the only place the desktop's
`env_secrets_from_request` scrubber looks. A flat `model_env` is accepted but loses that second
redaction layer.

`deploy` provisions and starts the unit, and returns `{"ok": true, "agent_id": "buzz-acp@<slug>"}`.
The desktop persists `agent_id` in `record.backend_agent_id`.

Errors are `{"ok": false, "error": "…"}` on stdout, human detail on stderr, and **exit 0 always**.
A non-zero exit makes `invoke_provider` discard stdout entirely and report raw stderr, which throws
the structured error away.

## Configuration

`validate_provider_config` rejects any config key whose word-split contains
`secret`/`password`/`token`/`key`/`credential`, and drops it silently. That is why the identity
field is `ssh_identity_file` and not `ssh_key_path`.

| key | required | notes |
|---|---|---|
| `ssh_host` | yes | hostname, IP, or `user@host`. Rejected if it starts with `-` or contains whitespace/control characters. Carries a `oneOf` of tailnet devices when one is available. |
| `ssh_user` | no | Ignored when `ssh_host` already contains `@`. |
| `ssh_port` | no | Number or numeric string, `1..=65535`. Default 22. |
| `ssh_identity_file` | no | Passed as `ssh -i`. Defaults to `~/.ssh/config` and the agent. |
| `buzz_acp_path` | no | Absolute path to `buzz-acp` on the host. Defaults to whatever is on the host's PATH. |

There is no `unit_scope`. All deploys are `systemctl --user`.

## Host prerequisites

`scripts/provision-buzz-host.sh` checks all of these on a candidate host and prints what is
missing. It is a preflight, not an installer.

1. **A non-root user.** The whole flow is root-free. The env file lands under that user's
   ownership, beside the harness credentials that already live there (`~/.claude`,
   `~/.config/goose`).

2. **`loginctl enable-linger <user>`.** This is the one non-obvious prerequisite. Without lingering,
   the user manager is torn down when the last session ends, so the agent is killed the moment the
   deploy's own SSH session closes — which reads as a flaky agent, not as a configuration problem.
   Lingering also creates `/run/user/$(id -u)`, without which every `systemctl --user` call fails
   to reach the bus. `deploy` runs `loginctl enable-linger` itself, before any bus traffic, but
   best-effort: some hosts gate it behind polkit, and failing it must not fail an otherwise good
   deploy. On those hosts, run it once by hand as root.

3. **`buzz-acp` on the host's PATH**, conventionally `~/.local/bin/buzz-acp`, or an absolute path in
   `buzz_acp_path`. `discover_harnesses` reports its absence without failing; `deploy` refuses.

4. **At least one harness CLI**, named exactly as `discover_harnesses` probes it — the ACP adapter,
   not the vendor CLI. `claude-agent-acp` or `claude-code-acp` for Claude Code, `codex-acp` for
   Codex, `goose` for Goose, `cursor-agent`, `omp`, `grok`, `opencode`, `kimi`, `amp-acp`,
   `hermes-acp`, `openclaw`, or `buzz-agent`.

5. **SSH key auth.** Every invocation is `BatchMode=yes`, so a password prompt is an immediate
   failure and never a hang. Add the desktop machine's public key to `~/.ssh/authorized_keys`, or
   run `tailscale set --ssh` on the host.

6. **Tailscale (optional).** When the desktop's own `tailscale status --json` reports
   `BackendState: "Running"`, its peers decorate the `ssh_host` field as a device picker. Phones and
   TVs are filtered out; `Self` is never offered. The label carries reachability and, when the peer
   advertises `sshHostKeys`, a `· Tailscale SSH` marker — that field's absence is the negative
   signal, not an unknown. Tailscale absent, logged out, or empty produces a schema byte-identical
   to the plain one; manual SSH is the unchanged fallback.

`XDG_RUNTIME_DIR` needs no host action: a non-interactive SSH command often gets none, and `deploy`
sets it when the session did not supply one.

Windows hosts are never deploy targets. The provider runs on Windows — it resolves
`%SystemRoot%\System32\OpenSSH\ssh.exe` before PATH and suppresses the console window for every
child — but the remote side is POSIX `sh` and `systemd --user` throughout.

## Security invariants

These are properties of the code, not conventions to uphold.

- **Secrets cross on stdin only.** Every op sends its script to a remote `sh -s`; the remote argv is
  the literal string `sh -s`, and the local argv is ssh options. The remote `ps` is world-readable
  and the desktop's redaction has no reach there, so a secret on the remote argv would leak the
  agent identity to every user on the box.
- **The env file is owner-only.** Written under `umask 077`, `chmod 600`, then moved into place, so
  a failed write never leaves a half-written identity behind.
- **A deploy without the minted nsec fails closed.** An agent that mints its own key on the host
  looks deployed and is permanently unreachable: presence, mentions, `!shutdown`, badges and the
  NIP-OA auth tag all key off the pubkey the desktop minted.
- **A deploy without the harness pin fails closed.** The pin is the only channel by which the
  harness choice reaches the host. A blank one would fall through to `buzz-agent`, silently
  provisioning a harness the user never chose, so it is refused rather than substituted.
- **Reserved env keys are refused**, as are env names that are not POSIX identifiers and env values
  containing control characters. A newline in a value would otherwise end the assignment and start a
  line of the value's own choosing — including one that re-sets `BUZZ_PRIVATE_KEY`. The list is a
  verbatim copy of the desktop's `RESERVED_ENV_KEYS`, so a leak needs two independent failures.
- **`Secret` renders as `[REDACTED]`** in both `Debug` and `Display` and zeroizes on drop.
  `Agent` and `ssh::Output` deliberately do not derive `Debug` at all: the first holds provider API
  keys in plain `String`s, the second holds raw remote stderr, and only `Output::failure()` runs
  that through the scrubber.
- **Host-key trust is never relaxed for a typed address.** `StrictHostKeyChecking=ask` by default;
  `accept-new` only for an address this machine's own Tailscale daemon lists as a peer, which was
  already reached over a WireGuard-authenticated tunnel.
- **Provider binaries are resolved by discovery, never by name.** Every deploy, start and probe path
  resolves through `discover_provider_candidates`, so a frontend or IPC caller that names a
  `binaryPath` cannot steer execution at an arbitrary binary and feed it the agent's private key.

## The systemd unit

One templated `buzz-acp@.service` per host, instantiated per agent.

```ini
[Unit]
Description=Buzz agent %i
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
EnvironmentFile=%h/.config/buzz-acp/%i.env
ExecStart=@BUZZ_ACP_BIN@
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

- `StartLimitIntervalSec=0` — a long-running agent must never be rate-limited into staying down. A
  unit held by the start limiter looks exactly like an agent that silently died, and only
  `systemctl reset-failed` clears it.
- `EnvironmentFile` — holds the minted nsec; systemd reads it as the owning user.
- `ExecStart` is an absolute path, substituted at install time from the host's resolved `buzz-acp`.
  systemd does not expand environment variables in the program position, and the shell indirection
  that would work around that is not worth adding to a unit whose environment carries a private key.
  The substitution is shell parameter expansion, not `sed`: `sed -i` is a GNU extension that BSD and
  macOS hosts reject.

The instance name is derived from the agent name: lowercased, non-alphanumerics collapsed to `-`,
truncated to 32 characters, plus an 8-hex FNV-1a suffix of the original name. The suffix is not
decoration — the payload carries no stable agent identifier, so without it two agents whose names
differ only in punctuation would share one unit and one env file.

## Env file contract

The local spawn contract from `runtime.rs`, transcribed. Values resolved on the host — the absolute
harness path, `git-credential-nostr`, `PATH` — are appended by the remote script.

| var | value |
|---|---|
| `BUZZ_ACP_AGENT_COMMAND` | the pinned harness, resolved on the host with `command -v` |
| `PATH` | `$HOME/.local/bin:$PATH` |
| `BUZZ_PRIVATE_KEY` | payload `private_key_nsec` |
| `BUZZ_RELAY_URL`, `BUZZ_AUTH_TAG` | payload (auth tag omitted when absent) |
| `BUZZ_ACP_AGENT_ARGS` | comma-joined |
| `BUZZ_ACP_MCP_COMMAND` | empty |
| `BUZZ_ACP_LAZY_POOL` | `false` — always eager; lazy pair-start has no meaning for a unit systemd starts unconditionally |
| `BUZZ_ACP_AGENTS` | payload `parallelism` |
| `BUZZ_ACP_MULTIPLE_EVENT_HANDLING` | `steer` |
| `BUZZ_ACP_DEDUP` | `queue` |
| `BUZZ_ACP_RELAY_OBSERVER` | `true` |
| `BUZZ_ACP_RESPOND_TO` (+ `_ALLOWLIST`) | payload; `allowlist` mode with an empty list is refused |
| `BUZZ_ACP_SYSTEM_PROMPT`, `BUZZ_ACP_MODEL` | payload, omitted when empty |
| runtime model/provider env | payload `model` / `provider`, under the runtime's own names — see below |
| `BUZZ_ACP_IDLE_TIMEOUT`, `BUZZ_ACP_MAX_TURN_DURATION` | emitted only when set, so the harness's own defaults win |
| `NOSTR_PRIVATE_KEY`, `GIT_TERMINAL_PROMPT`, `GIT_CONFIG_*` | only when `git-credential-nostr` is on the host |
| user `env_vars` | written last, so they override — matching the local layering |

The runtime model/provider pair is the remote half of `runtime_metadata_env_vars`. `BUZZ_ACP_MODEL`
is what `buzz-acp` reads; these are what the harness underneath it reads, and local spawn writes
both. Without them a remote Goose would fall back to whatever `~/.config/goose/config.yaml` on the
host says, with the user's model pick silently ignored.

| pinned command | model var | provider var |
|---|---|---|
| `goose` | `GOOSE_MODEL` | `GOOSE_PROVIDER` |
| `buzz-agent` | `BUZZ_AGENT_MODEL` | `BUZZ_AGENT_PROVIDER` |

The lookup is keyed on the command's file name, so an absolute pin
(`/home/ubuntu/.local/bin/goose`) resolves to the same runtime — matching `known_acp_runtime`
locally. Runtimes absent from the table declare no such vars in `KNOWN_ACP_RUNTIMES` either:
Claude is `provider_locked`, and neither Claude nor Codex has a model env var. An unset payload
field writes no key at all.

`BUZZ_MANAGED_AGENT` is deliberately absent. It is the desktop's process-ownership marker for
reclaiming orphaned local children; where systemd owns the lifecycle it would be actively
misleading.

`turn_timeout_seconds` is deliberately never read. The payload still carries it, but
`BUZZ_ACP_TURN_TIMEOUT` is deprecated and ignored by the harness, and local spawn does not write it
either — `idle_timeout_seconds` and `max_turn_duration_seconds` are the live controls. A test pins
that no `TURN_TIMEOUT` key can reappear in the env file.

## Lifecycle

**Deploy is the start path.** `start_managed_agent` re-enters `deploy_to_provider`, so start and
redeploy are one code path and everything in it is idempotent. Non-idempotence would surface as
duplicate units, not as an error. One deploy is one round trip that resolves `buzz-acp` and the
harness, writes the env file atomically, enables lingering, installs the unit template,
`daemon-reload`s only when the unit content actually changed, then `enable --now` and `restart`.
The restart is what makes an already-running unit adopt the rewritten env file.

**Stop is `!shutdown`.** The desktop's `stop_managed_agent` command rejects non-local agents
outright. The frontend sends a signed `!shutdown` @mention; the harness consumes it, drains
in-flight prompts, publishes `offline` presence, and exits. `Restart=always` then restarts the unit
after `RestartSec=5` — a `!shutdown` stops the current process, not the unit. To stop the unit,
`systemctl --user stop buzz-acp@<slug>.service` on the host.

**There is no `undeploy` op.** Deleting a deployed remote agent requires `force_remote_delete: true`
and permanently orphans a systemd unit and an env file containing an nsec on the host. This is the
strongest candidate for the immediate follow-up PR.

**Logs** are `journalctl --user -u buzz-acp@<slug> -f` on the host.

## Troubleshooting

**"Failed to connect to bus" during deploy.** Lingering is off and `loginctl enable-linger` was
rejected (polkit), so `/run/user/$UID` does not exist and no `systemctl --user` call can reach the
user manager. Run `sudo loginctl enable-linger <user>` once and redeploy.

**Agent goes online, then offline as soon as the deploy finishes.** Same cause, softer symptom: the
bus was reachable through the deploy's own session, and the user manager was torn down with it.
Enable lingering.

**`buzz-acp not found on the server's PATH` (exit 90).** `command -v buzz-acp` failed on the host.
Install it to `~/.local/bin`, or set `buzz_acp_path`. Note that `discover_harnesses` reports this
non-fatally, so it can first appear at deploy time.

**`harness <name> not found on the server's PATH` (exit 91).** The pinned harness is not installed
under that name. Deploy stops before writing anything — no env file, no unit. Install the ACP
adapter and re-run discovery so the pin names a binary that exists.

**`Permission denied (publickey)`.** `BatchMode=yes` means SSH declined rather than prompting. Add
the public key to `~/.ssh/authorized_keys`, or `tailscale set --ssh` on the host.

**`Host key verification failed`.** The host key is not in `known_hosts`, and `BatchMode` cannot
prompt to accept it. Connect once with `ssh` by hand to review and accept the key. This is expected
for any manually typed address; tailnet peers are exempt.

**The device dropdown disappeared.** `tailscale status --json` no longer reports
`BackendState: "Running"` — most often a logged-out daemon, which exits 0 with
`BackendState: "NeedsLogin"`. The field degrades to plain text and manual SSH still works; a
MagicDNS name typed into it will fail with `Could not resolve hostname`.

## Known limitations

- `runtimeSupportsLlmProviderSelection` is a hardcoded id test (`buzz-agent` or `goose`). A remote
  harness whose id matches gets the LLM-provider selector; any other remote id does not, however
  the host's own catalog describes it.
- `BUZZ_ACP_TEAM_INSTRUCTIONS` is not carried to the host — the deploy payload has no team field, so
  a team-linked remote agent silently loses its team instructions.
- `MCP_HOOK_SERVERS` is not emitted. `mcp_hooks` is local catalog metadata the provider cannot
  compute, so remote agents have no `_Stop`/`_PostCompact` hook tools.
- `check` is implemented but has no desktop caller. `discover_harnesses` serves as the de facto
  preflight, since it is the first op the create flow runs against a host.
- The tailnet device picker filters out phones and TVs, but still offers Windows peers, which
  cannot be deploy targets. Picking one fails at deploy, not at selection.
- Nothing installs `buzz-acp` on the host. `discover_harnesses` reports its absence, `deploy`
  refuses with exit 90, and the create dialog's "the deploy will install it" copy overstates what
  the provider does.
