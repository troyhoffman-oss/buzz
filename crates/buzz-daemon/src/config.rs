//! Daemon configuration and the (relay, identity) socket-path derivation.
//!
//! Implements `DESIGN.md` §2.2 ("One daemon per (relay, identity)") and the
//! path halves of §2.5 and §6.5.
//!
//! The socket path is
//! `$XDG_RUNTIME_DIR/buzz/<hash>.sock` on Linux and
//! `~/Library/Application Support/buzz/run/<hash>.sock` on macOS, where
//! `<hash> = sha256(relay_url + ":" + pubkey + ":" + auth_tag_owner)[0..16]`
//! and `auth_tag_owner` is the owner pubkey from the NIP-OA auth tag, or `""`
//! when there is none.
//!
//! **The auth tag is part of the identity, not decoration.** Under NIP-OA the
//! *effective* identity is (agent pubkey, owner attestation): the same key with
//! and without a `BUZZ_AUTH_TAG`, or with two different tags, has different
//! relay permissions and different reachable channels. Omitting it from the
//! preimage would collide two distinct effective identities onto one socket,
//! one cache, and one read-state slot. The **full preimage is stored in the
//! pidfile JSON** so a support conversation can identify a collision by reading
//! it rather than by guessing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{DaemonError, Result};

/// Hard cap on live daemons per user (§2.2). The 11th spawn fails with
/// `daemon_limit_reached` naming the remedy: `buzz-tui daemon list`.
///
/// The bound is mechanical, not estimated: this fork has already been burned
/// once by an unbounded per-process population (48 cold ACP bridges at ~2.8 GB).
pub const MAX_LIVE_DAEMONS: usize = 10;

/// Default idle shutdown window (§2.3). `--idle-timeout 0` disables it, and the
/// VPS install (§6.5) sets `0`, because on the agent host the daemon *is* the
/// always-on archive.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Default byte budget for the decrypted-observer-frame cache (§2.4 [D-3]).
///
/// The cache is sized in **bytes, not frames** — frame *count* is not a memory
/// bound when a frame carries an arbitrary-size payload. 32 concurrent agents ×
/// 3000 frames × `OBSERVER_MAX_PLAINTEXT_LEN` would be a ~6 GB worst case.
pub const DEFAULT_OBSERVER_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// The (relay, identity) tuple that names one daemon (§2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocketIdentity {
    /// Relay websocket URL.
    pub relay_url: String,
    /// The daemon's own pubkey, lowercase hex.
    pub pubkey: String,
    /// Owner pubkey from the NIP-OA auth tag, or empty when there is none.
    /// Part of the identity, not decoration — see the module docs.
    pub auth_tag_owner: String,
}

impl SocketIdentity {
    /// Build an identity from its three parts.
    pub fn new(
        relay_url: impl Into<String>,
        pubkey: impl Into<String>,
        auth_tag_owner: impl Into<String>,
    ) -> Self {
        Self {
            relay_url: relay_url.into(),
            pubkey: pubkey.into(),
            auth_tag_owner: auth_tag_owner.into(),
        }
    }

    /// The full hash preimage. Stored verbatim in the pidfile JSON (§2.2) so a
    /// collision can be diagnosed by reading it rather than by guessing.
    pub fn preimage(&self) -> String {
        format!("{}:{}:{}", self.relay_url, self.pubkey, self.auth_tag_owner)
    }

    /// `sha256(preimage)[0..16]` as lowercase hex — the `<hash>` of §2.2.
    ///
    /// `[0..16]` is 16 **hex characters** (8 bytes), which is what the design's
    /// socket filenames show.
    pub fn hash(&self) -> String {
        let digest = Sha256::digest(self.preimage().as_bytes());
        hex::encode(digest)[..16].to_string()
    }

    /// Absolute socket path for this identity under `runtime_dir`.
    pub fn socket_path(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(format!("{}.sock", self.hash()))
    }

    /// Absolute spawn-race lock path for this identity (§2.3). Distinct from
    /// the socket so `flock` never contends with `connect`.
    pub fn lock_path(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(format!("{}.lock", self.hash()))
    }

    /// Absolute pidfile path for this identity. The registry directory is
    /// enumerated by `GET /daemon/registry` (§2.2).
    pub fn pidfile_path(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(format!("{}.json", self.hash()))
    }

    /// Per-identity SQLite cache path, `~/.local/share/buzz/<hash>/cache.db`
    /// (§4.1.1 deliverable 3). The file is `0600` in a `0700` directory.
    pub fn cache_path(&self, data_dir: &Path) -> PathBuf {
        data_dir.join(self.hash()).join("cache.db")
    }
}

/// Resolve the platform runtime directory that holds sockets and pidfiles
/// (§2.2). Linux uses `$XDG_RUNTIME_DIR/buzz`; macOS uses
/// `~/Library/Application Support/buzz/run`.
///
/// The directory is **not** created here — [`ensure_private_dir`] does that, so
/// a read-only caller (`daemon list`) never has the side effect.
pub fn runtime_dir() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let base = dirs::data_dir()
            .ok_or_else(|| DaemonError::Path("no application support directory".into()))?;
        Ok(base.join("buzz").join("run"))
    } else {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| DaemonError::Path("XDG_RUNTIME_DIR is not set".into()))?;
        Ok(base.join("buzz"))
    }
}

/// Resolve the data directory that holds caches and identities:
/// `~/.local/share/buzz` (§2.5, §4.1.1 deliverable 3).
pub fn data_dir() -> Result<PathBuf> {
    let base =
        dirs::data_dir().ok_or_else(|| DaemonError::Path("no data directory available".into()))?;
    Ok(base.join("buzz"))
}

/// Identity directory, `~/.local/share/buzz/identity` (§2.5). Holds
/// `<pubkey8>.ncryptsec` and `<pubkey8>.authtag`, `0600` in a `0700` directory.
pub fn identity_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("identity"))
}

/// Create `dir` (and parents) with mode `0700`, tightening it if it already
/// exists with looser bits.
///
/// §2.5 makes filesystem permissions the authorization model, so this is a
/// precondition for binding the socket, not a nicety.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir)?.permissions();
        if perms.mode() & 0o077 != 0 {
            perms.set_mode(0o700);
            std::fs::set_permissions(dir, perms)?;
        }
    }
    Ok(())
}

/// Reject a socket whose **parent directory** is group- or world-writable.
///
/// §2.5/§6.5: the `ssh -L` forward endpoint creates the local socket with the
/// process umask, not `0600`. A world-writable parent hands a fully
/// authenticated Buzz session — observer plaintext included — to anyone else on
/// the laptop, and no daemon-side peercred check can see them. `buzz-tui
/// --socket` applies the same rule client-side.
pub fn assert_socket_dir_is_private(socket: &Path) -> Result<()> {
    let dir = socket
        .parent()
        .ok_or_else(|| DaemonError::Path(format!("{} has no parent", socket.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(DaemonError::UnsafeSocketDir {
                path: socket.display().to_string(),
            });
        }
    }
    Ok(())
}

/// Resolved daemon configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The (relay, identity) tuple this daemon serves.
    pub identity: SocketIdentity,
    /// Absolute path of the listening socket.
    pub socket: PathBuf,
    /// Runtime directory holding the socket, lock, and pidfile.
    pub runtime_dir: PathBuf,
    /// Data directory holding the SQLite cache and identity files.
    pub data_dir: PathBuf,
    /// Idle shutdown window; `None` disables it (`--idle-timeout 0`, §2.3).
    pub idle_timeout: Option<Duration>,
    /// Byte budget for the decrypted-frame cache (§2.4 [D-3]).
    pub observer_cache_bytes: u64,
    /// True when the daemon was launched by `systemd --user`. Recorded in the
    /// pidfile so `buzz-tui daemon restart` prints
    /// `systemctl --user restart buzz-daemon` rather than SIGTERM-ing a unit
    /// systemd will resurrect underneath it (§2.3).
    pub systemd_managed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> SocketIdentity {
        SocketIdentity::new("wss://relay.example", "aa".repeat(32), "")
    }

    #[test]
    fn hash_is_sixteen_hex_chars() {
        let h = identity().hash();
        assert_eq!(h.len(), 16, "{h}");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "{h}");
    }

    /// §2.2: the auth tag is part of the identity. The same key with and
    /// without a tag must not collide onto one socket, one cache, and one
    /// read-state slot.
    #[test]
    fn auth_tag_owner_changes_the_hash() {
        let untagged = identity();
        let tagged = SocketIdentity::new(&untagged.relay_url, &untagged.pubkey, "bb".repeat(32));
        assert_ne!(untagged.hash(), tagged.hash());
    }

    /// §2.2: two *different* tags are two different effective identities.
    #[test]
    fn two_different_auth_tags_do_not_collide() {
        let base = identity();
        let a = SocketIdentity::new(&base.relay_url, &base.pubkey, "bb".repeat(32));
        let b = SocketIdentity::new(&base.relay_url, &base.pubkey, "cc".repeat(32));
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn relay_url_changes_the_hash() {
        let base = identity();
        let other = SocketIdentity::new("wss://other.example", &base.pubkey, "");
        assert_ne!(base.hash(), other.hash());
    }

    /// §2.2: the full preimage is stored in the pidfile so a collision is
    /// diagnosable by reading it.
    #[test]
    fn preimage_is_recoverable_from_its_parts() {
        let id = SocketIdentity::new("wss://r", "pk", "owner");
        assert_eq!(id.preimage(), "wss://r:pk:owner");
    }

    #[test]
    fn socket_lock_and_pidfile_share_the_hash_stem() {
        let id = identity();
        let dir = Path::new("/run/user/1000/buzz");
        let h = id.hash();
        assert_eq!(id.socket_path(dir), dir.join(format!("{h}.sock")));
        assert_eq!(id.lock_path(dir), dir.join(format!("{h}.lock")));
        assert_eq!(id.pidfile_path(dir), dir.join(format!("{h}.json")));
    }

    /// §2.5/§6.5: a world-writable parent directory is refused.
    #[cfg(unix)]
    #[test]
    fn world_writable_socket_dir_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fwd");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let sock = dir.join("x.sock");
        let err = assert_socket_dir_is_private(&sock).unwrap_err();
        assert_eq!(err.code(), "unsafe_socket_dir");
    }

    #[cfg(unix)]
    #[test]
    fn private_socket_dir_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("buzz");
        ensure_private_dir(&dir).unwrap();
        assert_socket_dir_is_private(&dir.join("x.sock")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_tightens_loose_modes() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("loose");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }
}
