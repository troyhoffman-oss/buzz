//! `buzz-daemon` entry point.
//!
//! Implements the daemon side of `DESIGN.md` §2.3 and the CLI surface of §2.5.
//!
//! **The user never types `buzz-daemon`** [LOCKED]. Spawn is a TUI startup step
//! (§2.3), and the flags below are what the TUI passes. They are documented
//! here anyway because §6.5's VPS install shape launches the daemon under
//! `systemd --user` directly, with `--idle-timeout 0` and
//! `--identity-credential`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use buzz_daemon::config::{
    Config, SocketIdentity, DEFAULT_IDLE_TIMEOUT, DEFAULT_OBSERVER_CACHE_BYTES,
};
use buzz_daemon::{identity, socket};

/// Local daemon that owns all Buzz protocol knowledge for terminal clients.
#[derive(Debug, Parser)]
#[command(name = "buzz-daemon", version, about)]
struct Cli {
    /// One-shot identity provisioning (§2.5). Absent means "serve".
    #[command(subcommand)]
    command: Option<Command>,

    /// Socket path to bind. Normally derived from the (relay, identity) tuple
    /// (§2.2) and passed by the TUI; an explicit path is for the `ssh -L`
    /// forward case of §6.5.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Relay websocket URL. Part of the socket-path preimage (§2.2).
    #[arg(long)]
    relay: Option<String>,

    /// Pubkey (or `<pubkey8>` prefix) of a provisioned identity.
    ///
    /// Resolves to `~/.local/share/buzz/identity/<pubkey8>.ncryptsec` and its
    /// `.authtag` sibling. This is the flag the TUI passes, and it exists so
    /// the front end never has to know the on-disk identity layout — §6.4 puts
    /// that knowledge in this crate, and a TUI spelling out the path would be
    /// a second implementation of it to keep in agreement.
    #[arg(long, conflicts_with_all = ["identity_ncryptsec", "auth_tag_file"])]
    identity: Option<String>,

    /// Path to the NIP-49 `ncryptsec` identity blob. **The path is on argv
    /// because it is not a secret**; the passphrase is not (§2.5).
    #[arg(long)]
    identity_ncryptsec: Option<PathBuf>,

    /// Read the passphrase as one line from stdin, then close stdin. This is
    /// the auto-spawn path (§2.5 path 1) and the default.
    #[arg(long)]
    passphrase_stdin: bool,

    /// Read the passphrase from a `systemd-creds` credential. The **unattended**
    /// path (§2.5 path 3): a systemd-launched daemon after a 03:00 reboot has no
    /// TTY and no attached client, so without this the always-on observer
    /// archive would silently run keyless in exactly the case it exists for.
    #[arg(long)]
    identity_credential: Option<String>,

    /// Read the passphrase from a `0600` file whose directory is not group- or
    /// world-writable (§2.5 path 3, file variant).
    #[arg(long)]
    passphrase_file: Option<PathBuf>,

    /// Path to the NIP-OA auth tag beside the ncryptsec (§2.5).
    ///
    /// A path, not a value: the tag is a *capability* credential rather than a
    /// secret, but it is identity-bound and part of the socket-path preimage
    /// (§2.2), so it belongs on disk beside the key rather than on a command
    /// line that appears in every process listing.
    #[arg(long)]
    auth_tag_file: Option<PathBuf>,

    /// Idle shutdown window in seconds; `0` disables it. The VPS install (§6.5)
    /// sets `0`, because on the agent host the daemon *is* the always-on
    /// archive (§2.3).
    #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT.as_secs())]
    idle_timeout: u64,

    /// Byte budget for the decrypted-observer-frame cache ([D-3]). Sized in
    /// **bytes, not frames** — frame count is not a memory bound when a frame
    /// carries an arbitrary-size payload.
    #[arg(long, default_value_t = DEFAULT_OBSERVER_CACHE_BYTES)]
    observer_cache_bytes: u64,

    /// Detach after binding the socket. Used by the TUI's spawn step (§2.3).
    #[arg(long)]
    detach: bool,
}

/// One-shot subcommands. Each runs, prints one JSON object, and exits — none of
/// them binds a socket or contacts a relay.
#[derive(Debug, Subcommand)]
enum Command {
    /// Provision an identity at rest (§2.5, "`buzz-tui identity import` — one
    /// shot, writes an ncryptsec to disk, then uses path 1").
    ///
    /// The request arrives as **one JSON line on stdin** and the response is
    /// one JSON object on stdout carrying a pubkey and a path — never key
    /// material. Nothing secret is on argv, per §2.5's opening rule.
    Identity {
        /// What to do.
        #[command(subcommand)]
        action: IdentityAction,
    },

    /// Print the socket, lock, and pidfile paths for a (relay, identity) pair.
    ///
    /// §2.2 derives them from
    /// `sha256(relay_url + ":" + pubkey + ":" + auth_tag_owner)[0..16]`, where
    /// `auth_tag_owner` is the owner pubkey **parsed out of a NIP-OA tag**. The
    /// TUI needs the socket path before any daemon is running, and §6.4 keeps
    /// tag parsing out of the front end — so it asks for the answer here rather
    /// than reimplementing the derivation. Two implementations of one hash is
    /// how a client and a daemon end up on two sockets for one identity, which
    /// is the double-daemon §2.3's lock exists to prevent.
    ///
    /// Costs no scrypt: the auth tag is read and verified, the blob is not
    /// opened, so this is a few milliseconds and safe to call on every launch.
    SocketPath {
        /// Relay websocket URL.
        #[arg(long)]
        relay: String,
        /// Pubkey (or `<pubkey8>` prefix) of a provisioned identity.
        #[arg(long)]
        identity: String,
    },
}

/// Identity subcommands.
#[derive(Debug, Subcommand)]
enum IdentityAction {
    /// Read `{"mode":"create"|"import", …}` from stdin, write the blob, print
    /// `{"pubkey", "ncryptsec_path", "created"}`.
    Provision,
    /// Print `{"identities": ["<pubkey8>", …]}` for the identity directory.
    ///
    /// This is how the TUI decides whether it is a first run: an empty list
    /// with no configured community *is* the brand-new-user state, and it is
    /// read from the daemon rather than guessed from the TUI's own config, so
    /// an operator who provisioned on another front end is not re-onboarded.
    List,
    /// Read `{"pubkey", "tag"}` from stdin and store the NIP-OA auth tag beside
    /// that identity's blob (§2.5).
    SetAuthTag,
}

/// `socket::bind` calls `tokio::net::UnixListener::bind`, which **panics**
/// without a reactor ("there is no reactor running"). A synchronous `main` made
/// every real invocation abort at the bind — exit 101, a panic message, and a
/// stale socket file left behind — while the unit tests passed, because each is
/// `#[tokio::test]` and therefore has a runtime the binary did not.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // §2.5: there is no environment-variable path. The daemon reads the
    // variable only to refuse it, *before* anything binds — a refusal after a
    // listener is up would leave a half-started daemon behind. It runs ahead of
    // the subcommand dispatch too: provisioning is exactly when a stray
    // `BUZZ_PRIVATE_KEY` would be most tempting to honour.
    identity::refuse_env_key_paths()?;

    if let Some(command) = &cli.command {
        return run_command(command);
    }

    // The identity flags parse but are not yet honoured (deliverable 2). Refuse
    // them rather than accepting them silently: §2.5 makes "keyless" a visible
    // state precisely because a daemon that looks keyed while archiving nothing
    // is the §1.3-property-3 failure. A flag that is accepted and ignored
    // creates exactly that appearance, and an operator debugging an empty
    // observer archive would have no way to see it from the outside.
    let idle_timeout = match cli.idle_timeout {
        0 => None,
        secs => Some(std::time::Duration::from_secs(secs)),
    };

    let runtime_dir = buzz_daemon::config::runtime_dir()?;
    let data_dir = buzz_daemon::config::data_dir()?;

    // §2.5: the identity is loaded **before** anything binds. A daemon that
    // bound its socket and then failed to decrypt would advertise itself as
    // available while archiving nothing — the §1.3-property-3 failure this
    // whole section exists to prevent — and a client attaching in that window
    // could not tell it apart from a healthy one.
    let identity = load_identity(&cli).await?;
    if identity.is_none() {
        // Keyless is a supported state (§2.5), but never a *quiet* one: it is
        // logged at startup and reported as `archiving: false` on every
        // `/health` for as long as it lasts.
        tracing::warn!(
            "starting without an identity: archiving is OFF and observer frames \
             will not decrypt until POST /session/identity loads a key"
        );
    }

    // §2.2: the socket path is derived from (relay, pubkey, auth-tag owner), so
    // two effective identities never collide onto one socket, one cache, and
    // one read-state slot. An explicit `--socket` overrides it for the `ssh -L`
    // forward case of §6.5.
    let identity_tuple = SocketIdentity::new(
        cli.relay.clone().unwrap_or_default(),
        identity
            .as_ref()
            .map(|i| i.pubkey.clone())
            .unwrap_or_default(),
        identity
            .as_ref()
            .and_then(|i| i.auth_tag.as_ref())
            .map(|t| t.owner_pubkey.clone())
            .unwrap_or_default(),
    );
    let socket_path = match cli.socket.clone() {
        Some(path) => path,
        None => {
            if identity.is_none() {
                // Deriving a path from an empty preimage would put every
                // keyless daemon on the *same* socket, which is a collision
                // that presents as one daemon mysteriously serving another's
                // cache. Refusing names the two ways out.
                return Err(
                    "cannot derive a socket path without an identity; pass \
                            --identity-ncryptsec with a passphrase source, or --socket explicitly"
                        .into(),
                );
            }
            buzz_daemon::config::ensure_private_dir(&runtime_dir)?;
            identity_tuple.socket_path(&runtime_dir)
        }
    };

    let config = Config {
        identity: identity_tuple,
        socket: socket_path,
        runtime_dir,
        data_dir,
        idle_timeout,
        observer_cache_bytes: cli.observer_cache_bytes,
        // §2.3: the marker lets `buzz-tui daemon restart` print
        // `systemctl --user restart buzz-daemon` rather than SIGTERM-ing a unit
        // systemd will resurrect underneath it.
        systemd_managed: std::env::var_os("INVOCATION_ID").is_some(),
    };

    tracing::info!(
        socket = %config.socket.display(),
        idle_timeout_secs = cli.idle_timeout,
        systemd_managed = config.systemd_managed,
        archiving = identity.is_some(),
        "buzz-daemon {} (api {})",
        buzz_daemon::VERSION,
        buzz_daemon::API_VERSION,
    );

    // Applies the 0700-directory and 0600-socket posture of §2.5, and refuses a
    // group- or world-writable parent (the `ssh -L` hole of §6.5).
    let listener = socket::bind(&config.socket)?;
    tracing::info!(uid = socket::daemon_uid(), "socket bound; peercred armed");

    let socket_path = config.socket.clone();
    let state = buzz_daemon::state::AppState::new(config, identity)?;

    // The relay loop, and the handle every write endpoint reaches it through.
    // Started **after** the state exists and **before** the router is built, so
    // no request can arrive at a handler whose `state.wire` is still `None` —
    // that would present as a spurious `503` in the first milliseconds of a
    // daemon's life, which reads exactly like a relay outage.
    let (wire, commands) = buzz_daemon::wire::channel();
    let state = state.with_wire(wire);
    let wire_state = state.clone();
    tokio::spawn(async move {
        buzz_daemon::wire::run(wire_state, commands).await;
    });

    let app = buzz_daemon::api::router(state.clone());

    // The idle timer keys on **client activity**, not connection presence
    // (§2.2): a detached tmux pane holding an `/event` stream open is the normal
    // state, not a live client.
    let idle_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if idle_state.should_idle_exit().await {
                tracing::info!("idle timeout elapsed; shutting down");
                std::process::exit(0);
            }
        }
    });

    let serve = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    let result = serve.await;

    // Unlink on the way out so the next spawn's liveness probe sees ENOENT
    // rather than a socket nothing is listening on. §2.3 probes the socket
    // rather than a pidfile pid precisely because the pid check is a reuse
    // race — leaving a stale socket behind makes that probe answer wrong.
    let _ = std::fs::remove_file(&socket_path);
    result?;
    Ok(())
}

/// Run a one-shot subcommand: read stdin, write one JSON line, exit.
///
/// Synchronous on purpose. Provisioning runs scrypt at log-n 18 (~256 MiB,
/// hundreds of milliseconds), and the argument for `spawn_blocking` in the
/// serving path — that it would wedge every other socket client — does not
/// apply to a process whose only job is this one call. Putting it on the async
/// runtime anyway would suggest a concurrency story that does not exist.
fn run_command(command: &Command) -> Result<(), Box<dyn std::error::Error>> {
    use buzz_daemon::provision;

    let identity_dir = buzz_daemon::config::identity_dir()?;

    match command {
        Command::Identity {
            action: IdentityAction::Provision,
        } => {
            // One line, so a caller can pipe a request without deciding when to
            // close stdin — which matters because the TUI writes this from a
            // key handler and a half-closed pipe would hang the wizard.
            let request: provision::ProvisionRequest = serde_json::from_str(&read_stdin_line()?)?;
            let outcome = provision::provision(&request, &identity_dir)?;
            println!("{}", serde_json::to_string(&outcome)?);
        }
        Command::Identity {
            action: IdentityAction::List,
        } => {
            let identities = provision::provisioned_identities(&identity_dir);
            println!(
                "{}",
                serde_json::json!({
                    "identities": identities,
                    "identity_dir": identity_dir.display().to_string(),
                })
            );
        }
        Command::Identity {
            action: IdentityAction::SetAuthTag,
        } => {
            #[derive(serde::Deserialize)]
            struct SetAuthTag {
                pubkey: String,
                tag: String,
            }
            let request: SetAuthTag = serde_json::from_str(&read_stdin_line()?)?;
            let path = provision::write_auth_tag(&identity_dir, &request.pubkey, &request.tag)?;
            println!(
                "{}",
                serde_json::json!({"auth_tag_path": path.display().to_string()})
            );
        }
        Command::SocketPath { relay, identity } => {
            let runtime_dir = buzz_daemon::config::runtime_dir()?;
            let socket_identity = SocketIdentity::new(
                relay.clone(),
                resolve_full_pubkey(&identity_dir, identity)?,
                auth_tag_owner(&identity_dir, identity)?,
            );
            println!(
                "{}",
                serde_json::json!({
                    "socket": socket_identity.socket_path(&runtime_dir).display().to_string(),
                    "lock": socket_identity.lock_path(&runtime_dir).display().to_string(),
                    "pidfile": socket_identity.pidfile_path(&runtime_dir).display().to_string(),
                    "runtime_dir": runtime_dir.display().to_string(),
                })
            );
        }
    }
    Ok(())
}

/// The **full** pubkey behind a `<pubkey8>` stem or a full pubkey.
///
/// The socket preimage takes the full pubkey (§2.2) and the blob's filename
/// carries eight characters of it, so a stem is expanded through the `.pub`
/// sidecar [`buzz_daemon::provision::pubkey_path_for`] writes. NIP-49 is
/// ciphertext, so the sidecar is the only place the remaining 56 characters
/// exist without the passphrase.
///
/// A stem with no readable sidecar is an **error**, not a fallback: a socket
/// path derived from a truncated preimage names a daemon that looks right and
/// shares nothing with the real one, which is worse than refusing to launch.
fn resolve_full_pubkey(
    identity_dir: &std::path::Path,
    pubkey: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use buzz_daemon::provision::{is_pubkey, pubkey_path_for};

    if is_pubkey(pubkey) {
        return Ok(pubkey.to_string());
    }
    let sidecar = pubkey_path_for(identity_dir, pubkey);
    match std::fs::read_to_string(&sidecar) {
        Ok(full) if is_pubkey(full.trim()) => Ok(full.trim().to_string()),
        _ => Err(format!(
            "cannot resolve {pubkey} to a full pubkey: {} is missing or malformed. \
             Pass the full 64-character pubkey, or re-provision this identity.",
            sidecar.display()
        )
        .into()),
    }
}

/// The NIP-OA owner pubkey for an identity, or `""` when it has no auth tag.
///
/// Verified rather than trusted: `AuthTag::load` runs `verify_auth_tag`, so a
/// tag naming an owner it cannot prove does not silently change which socket
/// this identity lands on. A malformed tag is an error rather than an empty
/// owner, because falling back to `""` would move a tagged identity onto the
/// untagged socket — two effective identities on one cache, which §2.2 names.
fn auth_tag_owner(
    identity_dir: &std::path::Path,
    pubkey: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let path = identity::authtag_path_for(identity_dir, pubkey);
    if !path.exists() {
        return Ok(String::new());
    }
    identity::assert_secret_file_is_private(&path)?;
    let raw = std::fs::read_to_string(&path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let parsed = nostr::PublicKey::from_hex(pubkey)?;
    Ok(identity::AuthTag::load(raw.trim(), &parsed, now)?.owner_pubkey)
}

/// Read exactly one line from stdin.
///
/// The same shape as [`identity::read_passphrase_from_stdin`] and for the same
/// reason: the line may carry a secret, so it is read once and not echoed.
/// Unlike that function this one does not zeroize, because `serde_json` will
/// copy the fields out into owned `String`s the moment it parses — a zeroizing
/// buffer here would protect one copy of three and imply a guarantee this path
/// does not make. The real guarantee is process lifetime: this binary parses,
/// seals, and exits.
fn read_stdin_line() -> Result<String, Box<dyn std::error::Error>> {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err("expected one JSON request on stdin".into());
    }
    Ok(line)
}

/// Load the identity from whichever of §2.5's three intake paths was requested.
///
/// Returns `None` when no identity source was given, which is the keyless state
/// — supported, and visible on `/health`.
async fn load_identity(
    cli: &Cli,
) -> Result<Option<buzz_daemon::identity::Identity>, Box<dyn std::error::Error>> {
    use buzz_daemon::identity::{CredentialSource, IntakePath};

    // `--identity <pubkey>` resolves the on-disk layout here rather than in the
    // caller (§6.4): the TUI passes a pubkey and never learns that identities
    // are `<pubkey8>.ncryptsec` with an `.authtag` sibling.
    let (resolved_ncryptsec, resolved_auth_tag) = match cli.identity.as_deref() {
        Some(pubkey) => {
            let dir = buzz_daemon::config::identity_dir()?;
            // Both a stem and a full pubkey name the same blob (the path helper
            // truncates), so no expansion is needed *here* — but the socket
            // preimage below does need the full one, and resolving once keeps
            // the two from disagreeing about which identity is being served.
            let blob = identity::ncryptsec_path_for(&dir, pubkey);
            if !blob.exists() {
                // Naming the directory is what makes this actionable: the
                // common cause is a pubkey typo, and the second is a
                // `$HOME`/`XDG_DATA_HOME` that differs from the one
                // provisioning wrote under.
                return Err(format!(
                    "no provisioned identity for {pubkey}: {} does not exist",
                    blob.display()
                )
                .into());
            }
            let tag = identity::authtag_path_for(&dir, pubkey);
            (Some(blob), tag.exists().then_some(tag))
        }
        None => (cli.identity_ncryptsec.clone(), cli.auth_tag_file.clone()),
    };

    let Some(ncryptsec_path) = resolved_ncryptsec else {
        // A passphrase source with no blob to decrypt is a misconfiguration
        // that would otherwise start a keyless daemon looking like a keyed one.
        if cli.passphrase_stdin
            || cli.identity_credential.is_some()
            || cli.passphrase_file.is_some()
        {
            return Err("a passphrase source was given without --identity-ncryptsec".into());
        }
        return Ok(None);
    };

    // The three paths of §2.5, and no fourth. The passphrase never arrives on
    // argv and never through the environment.
    let (path, passphrase) = match (
        cli.passphrase_stdin,
        cli.identity_credential.as_ref(),
        cli.passphrase_file.as_ref(),
    ) {
        (true, None, None) => (
            IntakePath::SpawnStdin { ncryptsec_path },
            buzz_daemon::identity::read_passphrase_from_stdin()?,
        ),
        (false, Some(name), None) => {
            let source = CredentialSource::SystemdCreds(name.clone());
            let passphrase = buzz_daemon::identity::read_passphrase_from_credential(&source)?;
            (
                IntakePath::Credential {
                    ncryptsec_path,
                    source,
                },
                passphrase,
            )
        }
        (false, None, Some(file)) => {
            let source = CredentialSource::File(file.clone());
            let passphrase = buzz_daemon::identity::read_passphrase_from_credential(&source)?;
            (
                IntakePath::Credential {
                    ncryptsec_path,
                    source,
                },
                passphrase,
            )
        }
        (false, None, None) => {
            return Err(
                "--identity-ncryptsec needs a passphrase source: --passphrase-stdin, \
                        --identity-credential, or --passphrase-file"
                    .into(),
            )
        }
        // Two sources is ambiguous, and picking one silently would mean an
        // operator's `--passphrase-file` being ignored in favour of a stdin
        // that was never written — a hang with no diagnosis.
        _ => return Err("give exactly one passphrase source".into()),
    };

    let auth_tag = match resolved_auth_tag.as_ref() {
        Some(path) => {
            buzz_daemon::identity::assert_secret_file_is_private(path)?;
            Some(std::fs::read_to_string(path)?.trim().to_string())
        }
        None => None,
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The scrypt decrypt runs on `spawn_blocking` inside `load_identity` —
    // at log-n 18 it is hundreds of milliseconds and ~256 MiB, and on a tokio
    // worker it would wedge every other socket client for the duration.
    Ok(Some(
        buzz_daemon::identity::load_identity(&path, passphrase, auth_tag, now).await?,
    ))
}
