use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

const STDERR_CAP: usize = 65536;
/// Provider responses should be small JSON objects. Cap stdout to prevent a
/// buggy or malicious provider from OOM-ing the desktop process.
const STDOUT_CAP: usize = 1_048_576; // 1 MB

/// Invoke a provider binary: write JSON to stdin, read JSON from stdout.
///
/// Reader threads stream lines/chunks over channels so the caller can receive
/// data as it arrives and time-box the wait. No `read_to_end` — if a provider
/// daemonizes or leaves descendants holding pipes open, the caller still gets
/// all data written before the child exited and returns without leaking threads
/// (the readers drop naturally when the sender is gone and the pipe closes or
/// the desktop process exits).
pub fn invoke_provider(
    binary: &Path,
    request: &serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let request_bytes = format!(
        "{}\n",
        serde_json::to_string(request).map_err(|e| e.to_string())?
    );

    let mut cmd = std::process::Command::new(binary);
    if let Some(home) = super::default_agent_workdir() {
        cmd.current_dir(home);
    }
    crate::util::configure_no_window(&mut cmd);
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", binary.display()))?;

    // Write request and close stdin immediately so the provider sees EOF.
    let stdin_result = if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(request_bytes.as_bytes())
    } else {
        Ok(())
    };

    // Stream stdout as raw chunks over a channel. The caller appends chunks
    // to a buffer and attempts incremental JSON parsing — no dependency on
    // newlines or EOF. If a descendant holds the pipe open after the provider
    // exits, the thread blocks on the next read — but the caller already has
    // the response data and proceeds. The thread is not joined; it terminates
    // when the pipe eventually closes or the process exits.
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 8192];
            let mut reader = BufReader::new(stdout);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdout_tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Drain stderr into a bounded channel. sync_channel(8) caps in-flight
    // chunks — the producer blocks when the buffer is full, applying natural
    // backpressure. The consumer drains during the try_wait loop and caps
    // total bytes at STDERR_CAP, so memory is bounded even for long-running
    // or malicious providers.
    let (stderr_tx, stderr_rx) = mpsc::sync_channel::<Vec<u8>>(8);
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 8192];
            let mut reader = BufReader::new(stderr);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stderr_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Bail early if stdin write failed — child may be in a bad state.
    if let Err(e) = stdin_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("stdin write failed: {e}"));
    }

    // Poll try_wait with a deadline, collecting stdout chunks and draining
    // stderr as data arrives. Incremental JSON parsing on stdout means we
    // capture the response even without a trailing newline or EOF.
    let timeout_secs = timeout.as_secs();
    let deadline = std::time::Instant::now() + timeout;
    let mut stdout_buf = Vec::new();
    let mut stderr_bytes = Vec::new();
    let exit_status = loop {
        // Drain stdout chunks (non-blocking), enforce byte cap.
        while stdout_buf.len() < STDOUT_CAP {
            match stdout_rx.try_recv() {
                Ok(chunk) => stdout_buf.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }
        // Drain stderr chunks (non-blocking), enforce byte cap.
        while stderr_bytes.len() < STDERR_CAP {
            match stderr_rx.try_recv() {
                Ok(chunk) => stderr_bytes.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                break status;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("provider timed out after {timeout_secs}s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait error: {e}"));
            }
        }
    };

    // Drain remaining stdout chunks buffered between last poll and child exit.
    // Keep draining until the channel disconnects (reader finished) or the
    // 2s deadline expires (descendant holding pipe open). Do NOT break on the
    // first timeout — a slightly delayed final chunk should still be captured.
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if stdout_buf.len() >= STDOUT_CAP {
            break;
        }
        let remaining = drain_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match stdout_rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(chunk) => stdout_buf.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // reader done
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Keep waiting until the full drain deadline expires.
                if std::time::Instant::now() >= drain_deadline {
                    break;
                }
            }
        }
    }

    // Final stderr drain (non-blocking, cap already enforced).
    while stderr_bytes.len() < STDERR_CAP {
        match stderr_rx.try_recv() {
            Ok(chunk) => stderr_bytes.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    stderr_bytes.truncate(STDERR_CAP);
    stdout_buf.truncate(STDOUT_CAP);

    let stderr = String::from_utf8_lossy(&stderr_bytes);
    let env_secrets = env_secrets_from_request(request);
    let env_secret_refs: Vec<&str> = env_secrets.iter().map(String::as_str).collect();
    let stderr_redacted = redact_secrets_with(&stderr, &env_secret_refs);

    let exit_info = exit_status
        .code()
        .map(|c| format!("exit code {c}"))
        .unwrap_or_else(|| "killed by signal".to_string());

    // Fail on non-zero exit regardless of stdout content. A provider that
    // crashes mid-deploy may flush partial JSON before dying — trusting that
    // output would be worse than surfacing the failure.
    let exited_ok = exit_status.success();
    if !exited_ok {
        return Err(match provider_stderr_notice(&stderr_redacted) {
            Some(stderr_snippet) => {
                format!("provider failed ({exit_info}). stderr: {stderr_snippet}")
            }
            None => format!("provider failed ({exit_info}, empty stderr)"),
        });
    }

    // Incremental JSON parse: try each line, then try the entire buffer.
    // Handles providers that emit JSON on a single line (common) as well as
    // providers that write JSON without a trailing newline.
    let stdout_str = String::from_utf8_lossy(&stdout_buf);
    let response: serde_json::Value = stdout_str
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .or_else(|| serde_json::from_str(stdout_str.trim()).ok())
        .ok_or_else(|| match provider_stderr_notice(&stderr_redacted) {
            Some(stderr_snippet) => format!(
                "provider produced no JSON response ({exit_info}). stderr: {stderr_snippet}"
            ),
            None => {
                format!("provider produced no JSON response ({exit_info}, empty stderr)")
            }
        })?;

    if response.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let error = response["error"].as_str().unwrap_or("unknown error");
        return Err(redact_secrets_with(error, &env_secret_refs));
    }

    // A successful op's stderr is not an error, but it is not nothing either:
    // providers write their non-fatal complaints there (today, deploy's
    // "this host has no buzz CLI" WARNING). Without this the buffer is dropped
    // on success and the warning is invisible. Log-only by design — the op
    // succeeded, and a warning is not a result.
    if let Some(notice) = provider_stderr_notice(&stderr_redacted) {
        tracing::warn!("provider {}: {notice}", binary.display());
    }

    Ok(response)
}

/// The reportable form of a provider's stderr: `None` when it holds nothing
/// but whitespace, otherwise the already-redacted text trimmed and capped at
/// 4 KiB. The cap walks back to a char boundary so a multi-byte character
/// straddling it cannot panic. Both the success path (which logs it) and the
/// two failure paths (which fold it into the returned error) go through here,
/// so the snippet a warning shows and the snippet an error reports are the
/// same text under the same cap.
fn provider_stderr_notice(stderr_redacted: &str) -> Option<&str> {
    let trimmed = stderr_redacted.trim();
    if trimmed.is_empty() {
        return None;
    }
    let end = (0..=trimmed.len().min(4096))
        .rev()
        .find(|&i| trimmed.is_char_boundary(i))
        .unwrap_or(0);
    Some(&trimmed[..end])
}

/// Split a config key into lowercase words on `_`, `-`, `.`, and camelCase boundaries.
///
/// Handles acronyms: consecutive uppercase runs stay together until a lowercase follows.
/// "apiKey" → ["api", "key"], "apiKEY" → ["api", "key"], "APIKey" → ["api", "key"],
/// "access_token" → ["access", "token"], "keyboard" → ["keyboard"],
/// "clientSecret" → ["client", "secret"].
fn split_config_key(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = key.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '_' || ch == '-' || ch == '.' {
            if !current.is_empty() {
                words.push(current.to_lowercase());
                current.clear();
            }
        } else if ch.is_uppercase() {
            // Start a new word on: (a) transition from lowercase to uppercase, or
            // (b) uppercase followed by lowercase (end of acronym run, e.g. "APIKey" → "API" + "Key").
            let prev_lower =
                !current.is_empty() && current.chars().last().is_some_and(|c| c.is_lowercase());
            let acronym_end = !current.is_empty()
                && current.chars().last().is_some_and(|c| c.is_uppercase())
                && chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            if prev_lower || acronym_end {
                words.push(current.to_lowercase());
                current.clear();
            }
            current.push(ch);
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        words.push(current.to_lowercase());
    }
    words
}

#[cfg(test)]
fn redact_secrets(s: &str) -> String {
    redact_secrets_with(s, &[])
}

/// Like the (test-only) prefix-only `redact_secrets`, but also redacts
/// every occurrence of each
/// `extras` entry verbatim. Used to scrub user-supplied env values out of
/// provider stderr/JSON-error text — providers may echo their request
/// back in failure messages, and persona/agent `env_vars` may carry API
/// keys that the desktop just persisted via `last_error`.
///
/// Entries shorter than 4 chars are skipped: too noisy to scrub blindly
/// (would match every short token in normal log output). Entries are
/// applied in decreasing length order so superstrings get scrubbed before
/// substrings — protects against partial overlap leaks.
fn redact_secrets_with(s: &str, extras: &[&str]) -> String {
    let mut result = s.to_string();

    // Extras: longest first to avoid partial-overlap leaks. We use
    // `str::replace` (single-pass, non-overlapping) instead of a `find` +
    // `replace_range` loop — the loop variant would never terminate if a
    // user-supplied env value happened to be a substring of the
    // replacement marker (e.g. value="REDACTED" or "EDACTE").
    let mut sorted: Vec<&str> = extras.iter().copied().filter(|v| v.len() >= 4).collect();
    sorted.sort_by_key(|v| std::cmp::Reverse(v.len()));
    sorted.dedup();
    for value in sorted {
        if !value.is_empty() {
            result = result.replace(value, "[REDACTED]");
        }
    }

    // Then prefix-based scrubbing. This loop *can* re-scan because each
    // replacement shortens the buffer past the matched prefix — the
    // replacement marker `[REDACTED]` does not contain `nsec1` or
    // `sprt_tok_`, so progress is guaranteed.
    for prefix in &["nsec1", "sprt_tok_"] {
        while let Some(pos) = result.find(prefix) {
            let end = result[pos..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .map(|i| pos + i)
                .unwrap_or(result.len());
            result.replace_range(pos..end, "[REDACTED]");
        }
    }
    result
}

/// Collect string values from `request["agent"]["env_vars"]` (if present)
/// to feed into [`redact_secrets_with`]. Returns an empty Vec if the
/// request shape doesn't match, which is fine — falls back to the default
/// prefix-based scrubbing.
fn env_secrets_from_request(request: &serde_json::Value) -> Vec<String> {
    request
        .get("agent")
        .and_then(|a| a.get("env_vars"))
        .and_then(|e| e.as_object())
        .map(|obj| {
            obj.values()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Public-in-crate helper: redact every non-empty value from `env` (plus
/// the standard nsec/sprt_tok prefix scrubbing) out of `s`. Used by
/// callers that already have a flat env map handy — e.g. model discovery
/// formatting child stderr into a frontend-visible error.
pub(crate) fn redact_env_values_in(
    s: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> String {
    let values: Vec<&str> = env
        .values()
        .filter(|v| !v.is_empty())
        .map(String::as_str)
        .collect();
    redact_secrets_with(s, &values)
}

/// Deploy an agent via provider binary. Returns the provider-assigned agent_id.
///
/// `request_id` is included for provider-side logging/correlation but is not
/// validated in the response — the stdin→stdout exchange is 1:1 per process.
pub fn provider_deploy(
    binary: &Path,
    agent: &serde_json::Value,
    provider_config: &serde_json::Value,
) -> Result<String, String> {
    let request = serde_json::json!({
        "op": "deploy",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "agent": agent,
        "provider_config": provider_config,
    });
    let resp = invoke_provider(binary, &request, Duration::from_secs(600))?;
    resp["agent_id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "deploy response missing agent_id".to_string())
}

/// Ask a provider which harnesses exist on the machine it deploys to.
///
/// This is the catalog the create dialog must pick from for a remote agent:
/// the local `KNOWN_ACP_RUNTIMES` probe describes THIS computer, and what is
/// installed here says nothing about what is installed there. The `command`
/// on the chosen entry becomes the create-time `agentCommand` pin, which is
/// the only channel by which the harness choice reaches the host (see
/// `deploy_payload_json`).
///
/// The read is side-effect-free on the remote host, but it is a network round
/// trip against a possibly distant machine, so the budget is generous.
pub fn provider_discover_harnesses(
    binary: &Path,
    provider_config: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let request = serde_json::json!({
        "op": "discover_harnesses",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "provider_config": provider_config,
    });
    invoke_provider(binary, &request, Duration::from_secs(60))
}

/// Ask a provider for the model catalog of one remote harness.
///
/// `agent` carries the harness env (API keys) under `env_vars` — the same
/// shape `deploy` uses — because that is the only key `invoke_provider`
/// scrubs from error surfaces (`env_secrets_from_request`). Passing model env
/// anywhere else would let a provider echo a credential back through an error
/// string unredacted.
pub fn provider_probe_models(
    binary: &Path,
    provider_config: &serde_json::Value,
    harness: &serde_json::Value,
    env_vars: &std::collections::BTreeMap<String, String>,
) -> Result<serde_json::Value, String> {
    let request = serde_json::json!({
        "op": "probe_models",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "provider_config": provider_config,
        "harness": harness,
        "agent": { "env_vars": env_vars },
    });
    invoke_provider(binary, &request, Duration::from_secs(150))
}

/// Validate provider_config: flat object, scalar values, no secret-like keys.
pub fn validate_provider_config(config: &serde_json::Value) -> Result<(), String> {
    let obj = config
        .as_object()
        .ok_or("provider_config must be a JSON object")?;
    if obj.len() > 20 {
        return Err("provider_config: max 20 fields".to_string());
    }
    let json_str = serde_json::to_string(config).unwrap_or_default();
    if json_str.len() > 65536 {
        return Err("provider_config: max 64KB".to_string());
    }
    // Split on separators AND camelCase boundaries, then check each word.
    // Catches: api_key, apiKey, access-token, clientSecret, etc.
    // Allows: keyboard, monkey_wrench (no forbidden word as a segment).
    let forbidden = ["secret", "password", "token", "key", "credential"];
    for (k, v) in obj {
        let words = split_config_key(k);
        for f in &forbidden {
            if words.iter().any(|w| w == f) {
                return Err(format!("provider_config: key '{}' looks like a secret", k));
            }
        }
        if v.is_object() || v.is_array() {
            return Err(format!(
                "provider_config: value for '{}' must be a scalar",
                k
            ));
        }
    }
    Ok(())
}

const PROVIDER_PREFIX: &str = "buzz-backend-";

/// Executable extensions a provider binary may carry on Windows.
///
/// `.cmd` and `.bat` are deliberately absent. `std::process::Command` runs
/// those through `cmd.exe`, which would put a shell-quoting surface in front of
/// a code path that pipes an agent's private key over stdin. `.exe` and `.com`
/// are launched directly by `CreateProcess` with no interpreter in between.
/// This is a security boundary, not a style preference.
#[cfg(any(windows, test))]
const SAFE_EXEC_EXTENSIONS: &[&str] = &["exe", "com"];

/// Windows' documented `PATHEXT` default, used when the variable is unset or empty.
#[cfg(any(windows, test))]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Parse `PATHEXT` into this machine's provider-executable allowlist:
/// `PATHEXT` ∩ `SAFE_EXEC_EXTENSIONS`, lowercased and without the leading dot.
///
/// Taking the intersection means a machine whose `PATHEXT` drops an extension
/// drops it here too, while `.cmd`/`.bat` can never re-enter through a
/// user-configured `PATHEXT`.
///
/// `PATHEXT` order is preserved: it is the order Windows itself resolves a
/// bare command name in, and [`ExecNaming::rank`] reuses it to break ties
/// between two files in one directory.
#[cfg(any(windows, test))]
fn allowed_exec_extensions_from(pathext: Option<&str>) -> Vec<String> {
    let raw = pathext
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PATHEXT);
    raw.split(';')
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| SAFE_EXEC_EXTENSIONS.contains(&ext.as_str()))
        .collect()
}

/// How this platform names an executable file.
///
/// An explicit two-variant seam rather than "an allowlist that may be empty":
/// an empty Windows allowlist (a `PATHEXT` of only `.CMD`/`.BAT`) means
/// *nothing* is executable, which is the opposite of the unix meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecNaming {
    /// Unix: no extension convention; the execute permission bit is the contract.
    NoExtension,
    /// Windows: the file name must end in one of these extensions (lowercased,
    /// no leading dot). See `SAFE_EXEC_EXTENSIONS`.
    ///
    /// Only constructed on Windows (and in tests, which exercise the Windows
    /// rule from any host) — the variant still has to exist everywhere so the
    /// derivation logic is compiled and tested once rather than per-platform.
    #[cfg_attr(not(windows), allow(dead_code))]
    Extensions(Vec<String>),
}

impl ExecNaming {
    /// Where `ext` (no leading dot) sits in this platform's preference order,
    /// or `None` when it does not name an executable at all.
    fn rank(&self, ext: &str) -> Option<usize> {
        match self {
            ExecNaming::NoExtension => None,
            ExecNaming::Extensions(allowed) => {
                allowed.iter().position(|a| a.eq_ignore_ascii_case(ext))
            }
        }
    }

    /// Does `ext` (no leading dot) name an executable on this platform?
    fn allows(&self, ext: &str) -> bool {
        self.rank(ext).is_some()
    }
}

/// The executable-naming rule for the running platform.
fn provider_exec_naming() -> ExecNaming {
    #[cfg(windows)]
    {
        ExecNaming::Extensions(allowed_exec_extensions_from(
            std::env::var("PATHEXT").ok().as_deref(),
        ))
    }
    #[cfg(not(windows))]
    {
        ExecNaming::NoExtension
    }
}

/// Derive a provider id from a directory entry's file name, or `None` when the
/// entry is not a usable provider.
///
/// Under [`ExecNaming::NoExtension`] the name after the prefix is the id
/// verbatim. Under [`ExecNaming::Extensions`] the name MUST end in an allowed
/// extension and that extension is stripped: `buzz-backend-ssh.exe` yields
/// `ssh`, because `ssh.exe` fails [`provider_id_is_valid`] and would leave
/// every Windows provider undeployable. `buzz-backend-ssh.cmd` yields `None`
/// — see `SAFE_EXEC_EXTENSIONS`.
fn provider_id_from_file_name(name: &str, naming: &ExecNaming) -> Option<String> {
    let stem = name.strip_prefix(PROVIDER_PREFIX)?;
    if stem.is_empty() {
        return None;
    }
    if *naming == ExecNaming::NoExtension {
        return Some(stem.to_string());
    }
    let (base, ext) = stem.rsplit_once('.')?;
    if base.is_empty() || !naming.allows(ext) {
        return None;
    }
    Some(base.to_string())
}

/// Provider ids must match `^[a-z0-9][a-z0-9_-]*$`: no path components, no
/// shell metacharacters, and no file extension.
fn provider_id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Enumerate PATH for buzz-backend-* executables. Returns (id, path) pairs.
/// Only includes files that are executable. Does NOT execute any binaries.
///
/// On macOS, GUI apps inherit a minimal PATH from launchd (`/usr/bin:/bin:/usr/sbin:/sbin`)
/// which excludes both the app bundle's `Contents/MacOS/` dir and `~/.local/bin`.
/// We augment the search with those directories so bundled and user-installed providers
/// are always discovered regardless of how the desktop was launched.
///
/// On Windows the executable extension is stripped from the id and constrained
/// to `SAFE_EXEC_EXTENSIONS` — see [`provider_id_from_file_name`].
pub fn discover_provider_candidates() -> Vec<(String, PathBuf)> {
    let naming = provider_exec_naming();
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();

    // Prepend the exe parent dir (Contents/MacOS/ in a .app bundle) so bundled
    // providers are found even when the process PATH is minimal.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let parent_buf = parent.to_path_buf();
            if !dirs.contains(&parent_buf) {
                dirs.insert(0, parent_buf);
            }
        }
    }

    // Also include ~/.local/bin — the conventional location for user-installed
    // provider binaries (symlinks created by install scripts).
    if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local").join("bin");
        if !dirs.contains(&local_bin) {
            dirs.push(local_bin);
        }
    }

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Collect this directory's matches before taking any, so the winner of
        // an in-directory tie is chosen by `PATHEXT` rank rather than by
        // `read_dir` order — which is filesystem-dependent, so `foo.exe` and
        // `foo.com` side by side would otherwise resolve differently run to
        // run. Sorting by name as well keeps the whole result reproducible.
        let mut found: Vec<(usize, String, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let id = provider_id_from_file_name(&name, &naming)?;
                let rank = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .and_then(|ext| naming.rank(ext))
                    .unwrap_or(usize::MAX);
                // Ids that `resolve_provider_binary` would reject are dropped
                // here so the catalog never advertises a provider that can
                // never be executed.
                (provider_id_is_valid(&id) && is_executable(&path)).then_some((rank, id, path))
            })
            .collect();
        found.sort();

        // Dedupe on the id, not the file name: on Windows two extensions map to
        // one id, and the earlier PATH entry must win as it would for any other
        // command lookup.
        for (_, id, path) in found {
            if seen.insert(id.clone()) {
                results.push((id, path));
            }
        }
    }
    results
}

/// Resolve a provider ID to a discovered, executable binary path.
///
/// This is the ONLY way to resolve provider binaries for execution. It:
/// 1. Validates the ID against `^[a-z0-9][a-z0-9_-]*$` (no path traversal)
/// 2. Looks up the ID in `discover_provider_candidates()` (PATH-discovered only)
/// 3. Returns the canonical path of the discovered binary
///
/// All deploy, start, and create paths MUST use this instead of raw
/// `resolve_command(format!("buzz-backend-{id}"))` to prevent a compromised
/// frontend/IPC caller from steering execution to an arbitrary binary.
pub fn resolve_provider_binary(provider_id: &str) -> Result<PathBuf, String> {
    // Reject IDs that could be path components or shell metacharacters.
    if !provider_id_is_valid(provider_id) {
        return Err(format!(
            "invalid provider ID '{provider_id}': must match [a-z0-9][a-z0-9_-]*"
        ));
    }

    let candidates = discover_provider_candidates();
    let found = candidates
        .into_iter()
        .find(|(id, _)| id == provider_id)
        .map(|(_, path)| path);

    match found {
        Some(path) => path
            .canonicalize()
            .map_err(|e| format!("provider binary not accessible: {e}")),
        None => Err(format!(
            "provider 'buzz-backend-{provider_id}' not found on PATH"
        )),
    }
}

/// Check if a file is executable.
///
/// Unix: a regular file with at least one execute mode bit.
/// Windows: a regular file whose extension is allowed by [`provider_exec_naming`]
/// — Windows has no execute bit, so the extension *is* the contract.
/// Other platforms: regular file only.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(windows)]
    {
        let naming = provider_exec_naming();
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| naming.allows(ext))
    }

    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendProviderInfo {
    pub id: String,
    pub binary_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_stderr_notice_skips_blank_and_keeps_warnings() {
        // The success path only logs when the provider actually said
        // something — a blank buffer must not produce an empty warn line.
        assert_eq!(provider_stderr_notice(""), None);
        assert_eq!(provider_stderr_notice("  \n\t "), None);
        assert_eq!(
            provider_stderr_notice("buzz-backend-ssh: WARNING: no buzz CLI\n"),
            Some("buzz-backend-ssh: WARNING: no buzz CLI")
        );
    }

    #[test]
    fn provider_stderr_notice_caps_on_a_char_boundary() {
        // A multi-byte char straddling the 4096-byte cap must not panic.
        let long = format!("{}é", "a".repeat(4095));
        let notice = provider_stderr_notice(&long).expect("non-empty");
        assert_eq!(notice.len(), 4095);
    }

    #[test]
    fn redact_secrets_replaces_nsec() {
        let s = "key=nsec1abc123def456 other";
        let r = redact_secrets(s);
        assert!(r.contains("[REDACTED]"));
        assert!(!r.contains("nsec1abc123def456"));
    }

    #[test]
    fn redact_secrets_replaces_token() {
        let s = r#"{"token":"sprt_tok_xyz789"}"#;
        let r = redact_secrets(s);
        assert!(r.contains("[REDACTED]"));
        assert!(!r.contains("sprt_tok_xyz789"));
    }

    #[test]
    fn redact_secrets_with_extras_scrubs_user_env_values() {
        // If a provider echoes back a user-supplied API key in its error
        // output, the desktop must not surface that secret unredacted via
        // `last_error`. We scrub the literal values that came from the
        // request's `agent.env_vars`.
        let secret = "sk-ant-api03-abc123def456";
        let stderr = format!("auth failed with key {secret} on host api.anthropic.com");
        let r = redact_secrets_with(&stderr, &[secret]);
        assert!(r.contains("[REDACTED]"));
        assert!(!r.contains(secret));
    }

    #[test]
    fn redact_secrets_with_extras_skips_short_values() {
        // Don't scrub values shorter than 4 chars — too noisy.
        let r = redact_secrets_with("error code: 42", &["42"]);
        assert!(r.contains("42"));
    }

    #[test]
    fn redact_secrets_with_extras_terminates_when_value_substring_of_marker() {
        // Regression: an earlier impl used `while let Some(pos) = find(value)`
        // which never terminates if the user's env value is a substring of
        // the replacement marker `[REDACTED]` — each replacement
        // reintroduces the same text. Now uses `str::replace` (single-pass).
        for value in ["REDACTED", "EDACTE", "REDA", "ACTED"] {
            let r = redact_secrets_with(&format!("leak={value}"), &[value]);
            assert!(r.contains("[REDACTED]"));
        }
    }

    #[test]
    fn redact_secrets_with_extras_handles_overlapping_secrets() {
        // Longer entries get scrubbed first so the substring "abc12" isn't
        // matched before "abc123" is consumed.
        let s = "key1=abc123 key2=abc12";
        let r = redact_secrets_with(s, &["abc12", "abc123"]);
        assert!(!r.contains("abc123"));
        assert!(!r.contains("abc12 "));
    }

    #[test]
    fn env_secrets_from_request_extracts_string_values() {
        let req = serde_json::json!({
            "op": "deploy",
            "agent": {
                "env_vars": {
                    "ANTHROPIC_API_KEY": "sk-ant-test",
                    "EMPTY": "",
                    "NUMERIC": 42,
                },
            },
        });
        let secrets = env_secrets_from_request(&req);
        assert!(secrets.iter().any(|v| v == "sk-ant-test"));
        // Empty and non-string values are filtered out.
        assert_eq!(secrets.len(), 1);
    }

    #[test]
    fn env_secrets_from_request_handles_missing_shape() {
        assert!(env_secrets_from_request(&serde_json::json!({})).is_empty());
        assert!(env_secrets_from_request(&serde_json::json!({"agent": {}})).is_empty());
        assert!(
            env_secrets_from_request(&serde_json::json!({"agent": {"env_vars": null}})).is_empty()
        );
    }

    #[test]
    fn redact_env_values_in_scrubs_map_values() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("ANTHROPIC_API_KEY".to_string(), "sk-ant-real".to_string());
        env.insert("EMPTY".to_string(), String::new());
        let stderr = "auth=sk-ant-real failed; other context";
        let r = redact_env_values_in(stderr, &env);
        assert!(!r.contains("sk-ant-real"));
        assert!(r.contains("[REDACTED]"));
    }

    #[test]
    fn validate_provider_config_rejects_secret_key() {
        let cfg = serde_json::json!({"api_key": "val"});
        assert!(validate_provider_config(&cfg).is_err());
    }

    #[test]
    fn validate_provider_config_rejects_nested() {
        let cfg = serde_json::json!({"region": {"us": "east"}});
        assert!(validate_provider_config(&cfg).is_err());
    }

    #[test]
    fn validate_provider_config_accepts_scalars() {
        let cfg = serde_json::json!({"region": "us-east-1", "tier": "standard"});
        assert!(validate_provider_config(&cfg).is_ok());
    }

    #[test]
    fn validate_provider_config_allows_key_as_substring() {
        // "keyboard", "monkey" contain "key" as substring but not as a word segment.
        let cfg = serde_json::json!({"keyboard_layout": "us", "monkey_wrench": "tight"});
        assert!(validate_provider_config(&cfg).is_ok());
    }

    #[test]
    fn validate_provider_config_rejects_camel_case_secrets() {
        assert!(validate_provider_config(&serde_json::json!({"apiKey": "val"})).is_err());
        assert!(validate_provider_config(&serde_json::json!({"accessToken": "val"})).is_err());
        assert!(validate_provider_config(&serde_json::json!({"clientSecret": "val"})).is_err());
        // ALL-CAPS variants
        assert!(validate_provider_config(&serde_json::json!({"apiKEY": "val"})).is_err());
        assert!(validate_provider_config(&serde_json::json!({"accessTOKEN": "val"})).is_err());
    }

    #[test]
    fn split_config_key_handles_all_styles() {
        assert_eq!(split_config_key("apiKey"), vec!["api", "key"]);
        assert_eq!(split_config_key("access_token"), vec!["access", "token"]);
        assert_eq!(split_config_key("keyboard"), vec!["keyboard"]);
        assert_eq!(split_config_key("client-secret"), vec!["client", "secret"]);
        // Acronym runs stay together
        assert_eq!(split_config_key("APIKey"), vec!["api", "key"]);
        assert_eq!(split_config_key("apiKEY"), vec!["api", "key"]);
        assert_eq!(split_config_key("accessTOKEN"), vec!["access", "token"]);
        assert_eq!(split_config_key("MyAPIKey"), vec!["my", "api", "key"]);
    }

    /// The naming rule a Windows host with a default `PATHEXT` produces. Lets
    /// the id-derivation tests exercise the Windows branch from any host —
    /// `provider_id_from_file_name` takes the rule as an argument precisely so
    /// this logic is testable without a Windows runner.
    fn windows_naming() -> ExecNaming {
        ExecNaming::Extensions(allowed_exec_extensions_from(None))
    }

    #[test]
    fn provider_id_strips_windows_executable_extension() {
        // The W1 bug: `buzz-backend-ssh.exe` yielded id "ssh.exe", which
        // `provider_id_is_valid` rejects on the `.`, so no provider was ever
        // runnable on Windows.
        let id = provider_id_from_file_name("buzz-backend-ssh.exe", &windows_naming());
        assert_eq!(id.as_deref(), Some("ssh"));
        assert!(provider_id_is_valid(id.as_deref().unwrap()));
        // Case-insensitive: Windows file names are routinely upper-cased.
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh.EXE", &windows_naming()).as_deref(),
            Some("ssh")
        );
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh.com", &windows_naming()).as_deref(),
            Some("ssh")
        );
        // Only the final extension is stripped — a dotted id stays invalid
        // rather than collapsing two files onto one id.
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh.v2.exe", &windows_naming()).as_deref(),
            Some("ssh.v2")
        );
        assert!(!provider_id_is_valid("ssh.v2"));
    }

    #[test]
    fn provider_id_rejects_shell_script_extensions_on_windows() {
        // `.cmd`/`.bat` route through cmd.exe, which would add shell quoting
        // in front of a stdin channel that carries an nsec. Security boundary.
        for name in [
            "buzz-backend-ssh.cmd",
            "buzz-backend-ssh.bat",
            "buzz-backend-ssh.CMD",
            "buzz-backend-ssh.ps1",
        ] {
            assert!(
                provider_id_from_file_name(name, &windows_naming()).is_none(),
                "{name} must not yield a provider id"
            );
        }
    }

    #[test]
    fn provider_id_rejects_extensionless_and_empty_names_on_windows() {
        // Windows requires an extension to execute, so a bare name is not a
        // provider there even though it is the normal case on unix.
        assert!(provider_id_from_file_name("buzz-backend-ssh", &windows_naming()).is_none());
        assert!(provider_id_from_file_name("buzz-backend-.exe", &windows_naming()).is_none());
        assert!(provider_id_from_file_name("buzz-backend-", &windows_naming()).is_none());
        assert!(provider_id_from_file_name("other-tool.exe", &windows_naming()).is_none());
    }

    #[test]
    fn provider_id_on_unix_keeps_the_name_verbatim() {
        // The unix arm must behave exactly like the old bare `strip_prefix`:
        // no extension handling at all.
        let unix = ExecNaming::NoExtension;
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh", &unix).as_deref(),
            Some("ssh")
        );
        assert_eq!(
            provider_id_from_file_name("buzz-backend-my_provider-2", &unix).as_deref(),
            Some("my_provider-2")
        );
        assert!(provider_id_from_file_name("buzz-backend-", &unix).is_none());
        assert!(provider_id_from_file_name("buzz-agent", &unix).is_none());
        // A dotted name on unix stays dotted and is then rejected as an id —
        // it is not silently normalized into a different provider.
        let dotted = provider_id_from_file_name("buzz-backend-ssh.exe", &unix);
        assert_eq!(dotted.as_deref(), Some("ssh.exe"));
        assert!(!provider_id_is_valid(dotted.as_deref().unwrap()));
    }

    #[test]
    fn allowed_exec_extensions_honors_pathext_and_never_admits_scripts() {
        // Unset/empty → documented Windows default, minus the script types.
        assert_eq!(allowed_exec_extensions_from(None), vec!["com", "exe"]);
        assert_eq!(allowed_exec_extensions_from(Some("  ")), vec!["com", "exe"]);
        // Intersection: a PATHEXT that omits .COM omits it here too.
        assert_eq!(
            allowed_exec_extensions_from(Some(".EXE;.BAT;.CMD")),
            vec!["exe"]
        );
        // A user-configured PATHEXT cannot re-admit shell scripts.
        let scripts_only = allowed_exec_extensions_from(Some(".CMD;.BAT;.PS1;.VBS"));
        assert!(scripts_only.is_empty());
    }

    #[test]
    fn empty_windows_allowlist_discovers_nothing_rather_than_everything() {
        // Regression guard for the seam itself: an empty Windows allowlist and
        // the unix rule are NOT the same thing. If `PATHEXT` admits no safe
        // extension, no file is a provider — the opposite of the unix arm,
        // where the absence of extensions means the name is the id.
        let empty = ExecNaming::Extensions(Vec::new());
        assert!(provider_id_from_file_name("buzz-backend-ssh.cmd", &empty).is_none());
        assert!(provider_id_from_file_name("buzz-backend-ssh.exe", &empty).is_none());
        assert!(provider_id_from_file_name("buzz-backend-ssh", &empty).is_none());
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh", &ExecNaming::NoExtension).as_deref(),
            Some("ssh")
        );
    }

    #[test]
    fn provider_id_is_valid_matches_resolve_provider_binary_rules() {
        assert!(provider_id_is_valid("ssh"));
        assert!(provider_id_is_valid("my_provider-2"));
        assert!(!provider_id_is_valid(""));
        assert!(!provider_id_is_valid("ssh.exe"));
        assert!(!provider_id_is_valid("MyProvider"));
        assert!(!provider_id_is_valid("-leading-dash"));
        assert!(!provider_id_is_valid("_leading_underscore"));
        assert!(!provider_id_is_valid("foo;rm -rf /"));
    }

    /// The whole discovery path against a real directory: `PATHEXT` ranking,
    /// id validation, executability and dedupe are unit-tested in isolation
    /// above, but only running `discover_provider_candidates` proves they
    /// compose — that a real `read_dir` entry survives all four and comes back
    /// as a usable (id, path) pair.
    #[cfg(unix)]
    #[test]
    fn discover_provider_candidates_finds_an_executable_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temp dir");
        let write = |name: &str, mode: u32| {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("chmod file");
            path
        };
        let provider = write("buzz-backend-zzztest", 0o755);
        // Non-executable, wrong prefix, and an id the resolver would reject —
        // each must be filtered by a different guard in the loop.
        write("buzz-backend-zzznotexec", 0o644);
        write("some-other-tool", 0o755);
        write("buzz-backend-ZZZUPPER", 0o755);

        // Prepended, so every other entry the process already had stays
        // visible and a concurrent test still resolves what it expects.
        let original_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{original_path}", dir.path().display()));
        let found = discover_provider_candidates();
        // The discovered pair is one `resolve_provider_binary` accepts — the
        // catalog never advertises a provider that cannot then be executed.
        let resolved = resolve_provider_binary("zzztest");
        std::env::set_var("PATH", &original_path);

        let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"zzztest"), "{ids:?}");
        assert!(!ids.contains(&"zzznotexec"), "{ids:?}");
        assert!(!ids.contains(&"ZZZUPPER"), "{ids:?}");
        assert_eq!(
            found.iter().find(|(id, _)| id == "zzztest").map(|(_, p)| p),
            Some(&provider)
        );
        assert_eq!(resolved.ok(), provider.canonicalize().ok());
    }

    #[test]
    fn same_directory_ties_are_broken_by_pathext_order_not_read_dir_order() {
        // `buzz-backend-ssh.exe` and `buzz-backend-ssh.com` in one directory
        // both yield id "ssh". Which one wins must not depend on the order the
        // filesystem happens to hand back, so the rank comes from `PATHEXT` —
        // the same order Windows itself resolves a bare command name in.
        let naming = windows_naming();
        assert_eq!(naming.rank("com"), Some(0));
        assert_eq!(naming.rank("exe"), Some(1));
        assert_eq!(naming.rank("cmd"), None);
        // A `PATHEXT` that lists .EXE first flips the preference with it.
        let exe_first = ExecNaming::Extensions(allowed_exec_extensions_from(Some(".EXE;.COM")));
        assert_eq!(exe_first.rank("exe"), Some(0));
        assert_eq!(exe_first.rank("com"), Some(1));
        // Unix has no extension preference at all.
        assert_eq!(ExecNaming::NoExtension.rank("exe"), None);
    }

    #[test]
    fn resolve_provider_binary_rejects_invalid_ids() {
        // Path traversal
        assert!(resolve_provider_binary("../evil").is_err());
        // Empty
        assert!(resolve_provider_binary("").is_err());
        // Uppercase
        assert!(resolve_provider_binary("MyProvider").is_err());
        // Spaces
        assert!(resolve_provider_binary("my provider").is_err());
        // Shell metacharacters
        assert!(resolve_provider_binary("foo;rm -rf /").is_err());
        // Valid format but not on PATH — should fail with "not found"
        assert!(resolve_provider_binary("nonexistent-test-id-12345").is_err());
    }

    #[test]
    fn resolve_provider_binary_accepts_valid_id_format() {
        // Valid ID format should pass validation. If the binary happens to
        // exist on PATH, Ok is returned; otherwise Err contains "not found"
        // (not "invalid provider ID"). Either outcome proves validation passed.
        match resolve_provider_binary("zzz-nonexistent-test-provider") {
            Ok(_) => {} // unlikely but fine — binary exists
            Err(e) => assert!(
                e.contains("not found"),
                "expected 'not found' error, got: {e}"
            ),
        }
    }
}
