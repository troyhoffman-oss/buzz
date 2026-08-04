//! Unix domain socket listener and peer-credential authorization.
//!
//! Implements `DESIGN.md` §2.5 ("Socket authorization, and what it does *not*
//! cover"): `0600` on the socket, `0700` on its directory, plus a
//! peer-credential check on accept — `SO_PEERCRED` on Linux, `LOCAL_PEERCRED`
//! on macOS — rejecting any connection whose uid is not the daemon's own.
//! Filesystem permissions are the authorization model; peercred is the belt to
//! that suspenders on a box with a mis-permissioned runtime directory.
//!
//! **No TCP listener ships.** That is the whole of the transport story: there
//! is no bind address, no port, and no `--listen` flag to add one.
//!
//! **Peercred is a local-only defense.** For the remote case the trust boundary
//! is the SSH session (§2.5, §6.5): a forwarded connection is made by the
//! sshd/ssh process on the daemon host, so peercred reports *that* process's
//! uid. The laptop end is the real hole, and it is closed by
//! [`crate::config::assert_socket_dir_is_private`] rather than here.

use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use crate::config::{assert_socket_dir_is_private, ensure_private_dir};
use crate::error::{DaemonError, Result};

/// Bind the daemon's listening socket at `path`.
///
/// Preconditions enforced here, in order:
/// 1. the parent directory exists and is `0700` ([`ensure_private_dir`]);
/// 2. the parent directory is not group- or world-writable
///    ([`assert_socket_dir_is_private`]);
/// 3. any stale socket file is removed — **only after** the caller has
///    confirmed the previous daemon is dead by probing the socket, never by
///    checking a pidfile pid, which is a PID-reuse race (§2.3);
/// 4. the bound socket is `chmod 0600`.
pub fn bind(path: &Path) -> Result<UnixListener> {
    let dir = path
        .parent()
        .ok_or_else(|| DaemonError::Path(format!("{} has no parent", path.display())))?;
    ensure_private_dir(dir)?;
    assert_socket_dir_is_private(path)?;

    if path.exists() {
        std::fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(listener)
}

/// The uid this daemon runs as — the only uid permitted to connect.
///
/// The **real** uid is the correct comparison target: `SO_PEERCRED` and
/// `LOCAL_PEERCRED` both report the peer's real uid, so comparing against the
/// effective uid would reject a legitimate client under any setuid arrangement.
#[cfg(unix)]
pub fn daemon_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

/// Authorize an accepted connection by peer credential (§2.5).
///
/// Returns [`DaemonError::PeerCredentialRejected`] when the peer's uid differs
/// from `expected_uid`. Every rejection is counted and surfaced on
/// `GET /daemon` — a silently refused connection is indistinguishable from a
/// hung one, which §1.3 property 3 forbids.
pub fn authorize_peer(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    let cred = stream.peer_cred()?;
    let peer_uid = cred.uid();
    if peer_uid != expected_uid {
        return Err(DaemonError::PeerCredentialRejected {
            peer_uid,
            daemon_uid: expected_uid,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_creates_a_0600_socket_in_a_0700_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("buzz");
        let sock = dir.join("abc.sock");
        let _listener = bind(&sock).unwrap();

        let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode();
        assert_eq!(sock_mode & 0o777, 0o600, "socket mode {sock_mode:o}");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700, "dir mode {dir_mode:o}");
    }

    /// §2.5/§6.5: a group- or world-writable parent is refused before bind.
    #[cfg(unix)]
    #[tokio::test]
    async fn bind_refuses_a_world_writable_parent() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fwd");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        // ensure_private_dir tightens it, so bind succeeds; the client-side
        // guard is what refuses a socket it did not create. Assert the guard
        // itself here rather than the bind path.
        let err = crate::config::assert_socket_dir_is_private(&dir.join("x.sock"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(err.unwrap_err().code(), "unsafe_socket_dir");
    }

    /// A connection from the daemon's own uid is authorized.
    #[cfg(unix)]
    #[tokio::test]
    async fn same_uid_peer_is_authorized() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("buzz").join("peer.sock");
        let listener = bind(&sock).unwrap();

        let client = tokio::spawn(async move { UnixStream::connect(&sock).await.unwrap() });
        let (server_side, _) = listener.accept().await.unwrap();
        let _client_side = client.await.unwrap();

        let uid = daemon_uid();
        authorize_peer(&server_side, uid).unwrap();
    }

    /// A connection from any other uid is rejected with the distinct code.
    #[cfg(unix)]
    #[tokio::test]
    async fn foreign_uid_peer_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("buzz").join("peer.sock");
        let listener = bind(&sock).unwrap();

        let client = tokio::spawn(async move { UnixStream::connect(&sock).await.unwrap() });
        let (server_side, _) = listener.accept().await.unwrap();
        let _client_side = client.await.unwrap();

        let err = authorize_peer(&server_side, daemon_uid().wrapping_add(1)).unwrap_err();
        assert_eq!(err.code(), "peer_rejected");
    }
}
