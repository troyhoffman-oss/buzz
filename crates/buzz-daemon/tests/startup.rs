//! Integration tests that drive the real `buzz-daemon` binary.
//!
//! These exist because of a class of defect the unit tests structurally cannot
//! see. Every unit test in this crate is a `#[tokio::test]`, so it runs with a
//! reactor already installed — which means a `main` that never starts one still
//! passes the entire suite while aborting on every real invocation. The gap is
//! not a missing assertion; it is that the unit tests never execute `main`.
//!
//! `CARGO_BIN_EXE_buzz-daemon` is set by cargo for integration tests, so these
//! always run against the binary built from the current source.

use std::path::Path;
use std::process::Command;

/// Where the daemon binary for this build lives.
fn daemon_bin() -> &'static str {
    env!("CARGO_BIN_EXE_buzz-daemon")
}

/// Create a `0700` scratch directory to bind inside.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "buzz-daemon-it-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

/// The socket binds without panicking.
///
/// Regression: `main` was synchronous while `socket::bind` calls
/// `tokio::net::UnixListener::bind`, which **panics** without a reactor. Every
/// real invocation aborted with "there is no reactor running", exit 101, and a
/// stale socket left on disk — while `cargo test` stayed green, because each
/// unit test supplies the runtime the binary did not.
///
/// The daemon still exits non-zero here (the serve loop is Wave 1), so the
/// assertion is on *how* it fails: the scaffold's own explanatory error, never
/// a panic.
#[test]
fn binding_the_socket_does_not_panic() {
    let dir = scratch("bind");
    let sock = dir.join("a.sock");
    let out = Command::new(daemon_bin())
        .arg("--socket")
        .arg(&sock)
        .output()
        .expect("spawn buzz-daemon");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("no reactor running"),
        "daemon panicked binding the socket: {stderr}"
    );
    assert!(!stderr.contains("panicked at"), "daemon panicked: {stderr}");
    assert_ne!(
        out.status.code(),
        Some(101),
        "101 is the Rust panic exit code: {stderr}"
    );
    assert!(
        stderr.contains("serve loop is not implemented yet"),
        "expected the scaffold's own error, got: {stderr}"
    );
}

/// §2.5: the bound socket is `0600`, whatever umask the caller had.
///
/// Regression: `bind(2)` applies the process umask, so this came out `0755`
/// under a default `0022` for the window before the chmod, and `0777` under a
/// permissive one. A unit test cannot observe the window; asserting the final
/// mode from a separate process with a hostile umask can.
#[cfg(unix)]
#[test]
fn the_bound_socket_is_0600_regardless_of_the_callers_umask() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    let dir = scratch("umask");
    let sock = dir.join("b.sock");
    let mut cmd = Command::new(daemon_bin());
    cmd.arg("--socket").arg(&sock);
    // SAFETY: `umask` is async-signal-safe and this closure runs in the child
    // between fork and exec, touching nothing else.
    unsafe {
        cmd.pre_exec(|| {
            nix::sys::stat::umask(nix::sys::stat::Mode::empty());
            Ok(())
        });
    }
    let _ = cmd.output().expect("spawn buzz-daemon");

    let mode = std::fs::metadata(&sock)
        .expect("socket was created")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "socket mode {mode:o} under umask 000");
}

/// §2.5: `BUZZ_PRIVATE_KEY` is read only to be refused, and the refusal happens
/// **before** anything binds — a refusal after a listener is up would leave a
/// half-started daemon and a live socket behind.
#[test]
fn a_secret_environment_variable_is_refused_before_the_socket_exists() {
    let dir = scratch("env");
    let sock = dir.join("c.sock");
    let out = Command::new(daemon_bin())
        .arg("--socket")
        .arg(&sock)
        .env("BUZZ_PRIVATE_KEY", "deadbeef")
        .output()
        .expect("spawn buzz-daemon");

    assert!(!out.status.success(), "daemon accepted BUZZ_PRIVATE_KEY");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("BUZZ_PRIVATE_KEY"),
        "refusal should name the variable: {stderr}"
    );
    assert!(
        !Path::new(&sock).exists(),
        "the refusal must precede the bind, but a socket was left at {}",
        sock.display()
    );
}
