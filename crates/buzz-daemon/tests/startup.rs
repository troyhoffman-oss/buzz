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

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Where the daemon binary for this build lives.
fn daemon_bin() -> &'static str {
    env!("CARGO_BIN_EXE_buzz-daemon")
}

/// A spawned daemon that is killed when the test ends.
///
/// The daemon **serves** now rather than exiting with an explanatory error, so
/// the tests that observe a *running* one cannot use `Command::output()` — that
/// waits for exit and hangs forever. This wrapper spawns, waits for the socket
/// to appear, and reaps on drop, so a failing assertion cannot leave a daemon
/// running on the machine.
struct Spawned {
    child: Child,
    socket: PathBuf,
}

impl Spawned {
    /// Spawn a daemon on `socket`, optionally clearing the umask in the child.
    fn start(socket: PathBuf, clear_umask: bool) -> Self {
        let mut cmd = Command::new(daemon_bin());
        cmd.arg("--socket")
            .arg(&socket)
            .arg("--relay")
            .arg("wss://relay.invalid")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        if clear_umask {
            use std::os::unix::process::CommandExt;
            // SAFETY: `umask` is async-signal-safe and this closure runs in the
            // child between fork and exec, touching nothing else.
            unsafe {
                cmd.pre_exec(|| {
                    nix::sys::stat::umask(nix::sys::stat::Mode::empty());
                    Ok(())
                });
            }
        }
        let child = cmd.spawn().expect("spawn buzz-daemon");
        let spawned = Self { child, socket };
        spawned.wait_for_socket();
        spawned
    }

    /// Block until the socket exists, or fail with a bounded, specific error.
    ///
    /// Bounded rather than unbounded: a daemon that fails to bind should make
    /// this test fail in seconds with "never appeared", not hang the suite.
    fn wait_for_socket(&self) {
        for _ in 0..200 {
            if self.socket.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("socket {} never appeared", self.socket.display());
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
/// The daemon now **serves** rather than exiting with an explanatory error, so
/// the assertion is that it stays up with a live socket. An earlier revision of
/// this test waited on `Command::output()`, which was correct while the binary
/// exited immediately and hangs forever now — worth recording, because a test
/// that hangs reads as a broken machine rather than a stale assertion.
#[test]
fn the_daemon_binds_and_stays_up() {
    let dir = scratch("bind");
    let mut daemon = Spawned::start(dir.join("a.sock"), false);

    // Still alive after binding — not a panic, not an early exit.
    assert!(
        daemon.child.try_wait().expect("poll child").is_none(),
        "the daemon exited instead of serving"
    );
    assert!(daemon.socket.exists());
}

/// The socket is a **socket**, not a regular file left behind by a crash.
///
/// Regression: `main` was synchronous while `socket::bind` calls
/// `tokio::net::UnixListener::bind`, which panics without a reactor. Every real
/// invocation aborted with "there is no reactor running" while `cargo test`
/// stayed green, because each unit test supplies the runtime the binary did
/// not. Connecting is what proves a reactor is actually running.
#[cfg(unix)]
#[test]
fn the_bound_socket_accepts_a_connection() {
    use std::os::unix::net::UnixStream;

    let dir = scratch("connect");
    let daemon = Spawned::start(dir.join("live.sock"), false);
    UnixStream::connect(&daemon.socket).expect("the daemon accepts connections");
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

    let dir = scratch("umask");
    let daemon = Spawned::start(dir.join("b.sock"), true);

    let mode = std::fs::metadata(&daemon.socket)
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
