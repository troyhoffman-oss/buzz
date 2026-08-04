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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    for (flag, present) in [
        ("--identity-ncryptsec", cli.identity_ncryptsec.is_some()),
        ("--passphrase-stdin", cli.passphrase_stdin),
        ("--identity-credential", cli.identity_credential.is_some()),
        ("--passphrase-file", cli.passphrase_file.is_some()),
        ("--detach", cli.detach),
    ] {
        if present {
            return Err(format!(
                "{flag} is not implemented yet (DESIGN.md §4.1.1 deliverable 2); \
                 refusing rather than starting a daemon that looks keyed but archives nothing"
            )
            .into());
        }
    }

    let idle_timeout = match cli.idle_timeout {
        0 => None,
        secs => Some(std::time::Duration::from_secs(secs)),
    };

    let runtime_dir = buzz_daemon::config::runtime_dir()?;
    let data_dir = buzz_daemon::config::data_dir()?;

    // The identity tuple is resolved from the decrypted key plus the NIP-OA
    // auth tag; until the identity path of deliverable 2 lands, an explicit
    // --socket is required so the process has an unambiguous bind target rather
    // than a guessed one.
    let socket_path = match cli.socket {
        Some(path) => path,
        None => {
            return Err(
                "resolving the socket path from the identity requires the Wave-1 identity \
                 loader (DESIGN.md §4.1.1 deliverable 2); pass --socket explicitly for now"
                    .into(),
            )
        }
    };

    let identity_tuple = SocketIdentity::new(
        cli.relay.clone().unwrap_or_default(),
        String::new(),
        String::new(),
    );

    let config = Config {
        identity: identity_tuple,
        socket: socket_path.clone(),
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
        "buzz-daemon {} (api {})",
        buzz_daemon::VERSION,
        buzz_daemon::API_VERSION,
    );

    // Binding is the one piece of the serve path that is real today: it applies
    // the 0700-directory and 0600-socket posture of §2.5 and refuses a
    // group- or world-writable parent (the `ssh -L` hole of §6.5).
    let _listener = socket::bind(&config.socket)?;
    tracing::info!(uid = socket::daemon_uid(), "socket bound; peercred armed");

    // TODO(wave1): serve `buzz_daemon::api::router()` over this listener with a
    // per-connection `socket::authorize_peer` gate, run the session layer of
    // §4.1.1 deliverable 1, and honour the idle timer of §2.2.
    Err("serve loop is not implemented yet (DESIGN.md §4.1.1 deliverables 1-14)".into())
}
