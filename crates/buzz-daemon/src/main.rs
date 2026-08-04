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

use clap::Parser;

use buzz_daemon::config::{
    Config, SocketIdentity, DEFAULT_IDLE_TIMEOUT, DEFAULT_OBSERVER_CACHE_BYTES,
};
use buzz_daemon::{identity, socket};

/// Local daemon that owns all Buzz protocol knowledge for terminal clients.
#[derive(Debug, Parser)]
#[command(name = "buzz-daemon", version, about)]
struct Cli {
    /// Socket path to bind. Normally derived from the (relay, identity) tuple
    /// (§2.2) and passed by the TUI; an explicit path is for the `ssh -L`
    /// forward case of §6.5.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Relay websocket URL. Part of the socket-path preimage (§2.2).
    #[arg(long)]
    relay: Option<String>,

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
    // listener is up would leave a half-started daemon behind.
    identity::refuse_env_key_paths()?;

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

/// Load the identity from whichever of §2.5's three intake paths was requested.
///
/// Returns `None` when no identity source was given, which is the keyless state
/// — supported, and visible on `/health`.
async fn load_identity(
    cli: &Cli,
) -> Result<Option<buzz_daemon::identity::Identity>, Box<dyn std::error::Error>> {
    use buzz_daemon::identity::{CredentialSource, IntakePath};

    let Some(ncryptsec_path) = cli.identity_ncryptsec.clone() else {
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

    let auth_tag = match cli.auth_tag_file.as_ref() {
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
