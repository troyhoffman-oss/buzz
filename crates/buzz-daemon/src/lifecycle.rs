//! Daemon lifecycle: pidfile registry, the population cap, and idle shutdown.
//!
//! Implements `DESIGN.md` §2.2 (the mechanical bound) and §2.3 (spawn, liveness
//! probing, and lifetime rules).
//!
//! # The user never types `buzz-daemon` [LOCKED]
//!
//! Spawn is a TUI startup step (§2.3). What lives here is the daemon side of
//! that handshake: the pidfile it writes, the registry
//! `GET /daemon/registry` enumerates, and the idle timer.
//!
//! # Why the registry exists
//!
//! §2.2: `/daemon/shutdown` without an enumeration path is a leak with no
//! broom. `buzz-tui daemon list` and `daemon stop --all` are Wave-1 surfaces,
//! reading a `0700` registry directory of pidfiles.
//!
//! # Liveness is probed on the socket, never on a pid
//!
//! §2.3: `connect()` refused/ENOENT means dead; connected-but-erroring means
//! alive. A pidfile pid check is a PID-reuse race and is not used. The socket is
//! unlinked **only on confirmed-dead**.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{SocketIdentity, MAX_LIVE_DAEMONS};
use crate::error::{DaemonError, Result};

/// The pidfile written next to the socket, and read by `GET /daemon/registry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidFile {
    /// Process id. Recorded for humans and for `daemon stop`; **never** used as
    /// a liveness probe (§2.3).
    pub pid: u32,
    /// Absolute socket path.
    pub socket: String,
    /// The (relay, identity) tuple this daemon serves.
    pub identity: SocketIdentity,
    /// The **full hash preimage** (§2.2), stored so a support conversation can
    /// identify a socket-path collision by reading it rather than by guessing.
    pub preimage: String,
    /// Daemon version, for the skew message of §2.3.
    pub version: String,
    /// Wire-contract version.
    pub api_version: u32,
    /// True when launched by `systemd --user`.
    ///
    /// §2.3: `buzz-tui daemon restart` must detect this and print
    /// `systemctl --user restart buzz-daemon` rather than SIGTERM-ing a unit
    /// systemd will resurrect underneath it.
    pub systemd_managed: bool,
}

impl PidFile {
    /// Build a pidfile record for `identity`.
    pub fn new(pid: u32, socket: &Path, identity: SocketIdentity, systemd_managed: bool) -> Self {
        Self {
            pid,
            socket: socket.display().to_string(),
            preimage: identity.preimage(),
            identity,
            version: crate::VERSION.to_string(),
            api_version: crate::API_VERSION,
            systemd_managed,
        }
    }
}

/// Enforce the per-user daemon cap of §2.2.
///
/// The 11th spawn fails with `daemon_limit_reached` naming the remedy. The
/// bound is mechanical, not estimated: this fork has already been burned once
/// by an unbounded per-process population.
pub fn check_daemon_cap(live: usize) -> Result<()> {
    if live >= MAX_LIVE_DAEMONS {
        return Err(DaemonError::DaemonLimitReached {
            cap: MAX_LIVE_DAEMONS,
        });
    }
    Ok(())
}

/// Whether an idle window has elapsed and the daemon should exit (§2.2).
///
/// **The idle timer keys on client *activity*, not on connection presence.** A
/// detached `tmux` pane holding an `/event` stream open is the normal state,
/// not a live client; a stream with no request in `--idle-timeout` counts as
/// idle. Without this the default 30-minute timer never fires for the exact
/// population this product targets.
///
/// `timeout: None` is `--idle-timeout 0` — the VPS install (§6.5) sets it,
/// because on the agent host the daemon *is* the always-on archive.
pub fn should_idle_exit(
    since_last_request: std::time::Duration,
    timeout: Option<std::time::Duration>,
) -> bool {
    match timeout {
        None => false,
        Some(t) => since_last_request >= t,
    }
}

/// Health payload served by `GET /health` (§2.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    /// Daemon semver.
    pub version: String,
    /// Wire-contract version. The client's rule is
    /// `daemon.api_version >= client.min_api_version` — a **floor**, not an
    /// equality, because exact-match would mean "restart the daemon to match
    /// your client", i.e. killing the always-on observer archive that §6.5's
    /// install shape exists for.
    pub api_version: u32,
    /// Implemented API groups (`channels`, `agents`, `projects`, …). At or
    /// above the floor, these decide which screens exist, so a Wave-3 TUI
    /// attached to a Wave-1 daemon *hides* what it cannot serve rather than
    /// erroring inside it (§2.3 [D-1]).
    pub capabilities: Vec<String>,
    /// **False when the daemon is running without an identity.** §2.5: a
    /// keyless daemon must never look identical to a healthy one — every
    /// attached TUI renders this in the status bar as a loss state.
    pub archiving: bool,
}

/// API groups this build implements. Wave 1 ships `channels` and `agents`; the
/// rest arrive in the wave that needs them and are **absent from
/// `capabilities[]` until then** (§2.4).
pub const WAVE1_CAPABILITIES: &[&str] = &["channels", "agents"];

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// §2.2: a hard cap of 10; the 11th spawn fails.
    #[test]
    fn the_eleventh_daemon_is_refused() {
        for live in 0..MAX_LIVE_DAEMONS {
            check_daemon_cap(live).unwrap();
        }
        let err = check_daemon_cap(MAX_LIVE_DAEMONS).unwrap_err();
        assert_eq!(err.code(), "daemon_limit_reached");
        assert!(err.to_string().contains("buzz-tui daemon list"), "{err}");
    }

    /// §2.2/§6.5: `--idle-timeout 0` disables the timer, which is what keeps
    /// the VPS archive always-on.
    #[test]
    fn zero_idle_timeout_never_exits() {
        assert!(!should_idle_exit(Duration::from_secs(86_400), None));
    }

    #[test]
    fn idle_exit_fires_at_the_boundary() {
        let t = Some(Duration::from_secs(1_800));
        assert!(!should_idle_exit(Duration::from_secs(1_799), t));
        assert!(should_idle_exit(Duration::from_secs(1_800), t));
    }

    /// §2.2: the full preimage is in the pidfile so a collision is diagnosable
    /// by reading it.
    #[test]
    fn pidfile_carries_the_full_preimage() {
        let identity = SocketIdentity::new("wss://r", "pk", "owner");
        let pidfile = PidFile::new(
            42,
            Path::new("/run/user/1000/buzz/abc.sock"),
            identity.clone(),
            false,
        );
        assert_eq!(pidfile.preimage, identity.preimage());
        assert_eq!(pidfile.api_version, crate::API_VERSION);
    }

    /// §2.3: the systemd marker is written at spawn so `daemon restart` prints
    /// the systemctl command instead of SIGTERM-ing a supervised unit.
    #[test]
    fn systemd_marker_round_trips() {
        let pidfile = PidFile::new(
            1,
            Path::new("/x.sock"),
            SocketIdentity::new("wss://r", "pk", ""),
            true,
        );
        let json = serde_json::to_string(&pidfile).unwrap();
        let back: PidFile = serde_json::from_str(&json).unwrap();
        assert!(back.systemd_managed);
    }

    /// §2.5: keyless is a *visible* state.
    #[test]
    fn health_reports_archiving_false_when_keyless() {
        let health = Health {
            version: crate::VERSION.into(),
            api_version: crate::API_VERSION,
            capabilities: WAVE1_CAPABILITIES.iter().map(|s| s.to_string()).collect(),
            archiving: false,
        };
        let json = serde_json::to_string(&health).unwrap();
        assert!(json.contains("\"archiving\":false"), "{json}");
    }

    /// §2.4: capabilities absent until the wave that implements them.
    #[test]
    fn wave1_capabilities_exclude_later_waves() {
        assert!(WAVE1_CAPABILITIES.contains(&"channels"));
        assert!(WAVE1_CAPABILITIES.contains(&"agents"));
        for later in ["projects", "moderation", "media", "workflows"] {
            assert!(!WAVE1_CAPABILITIES.contains(&later), "{later}");
        }
    }
}
