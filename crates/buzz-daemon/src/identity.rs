//! Identity: key custody, ncryptsec intake, and the NIP-OA auth tag.
//!
//! Implements `DESIGN.md` §2.5 and Wave-1 daemon deliverable 2 (§4.1.1).
//!
//! The governing discipline is already in this repo and the daemon follows it
//! exactly. `buzz-backend-ssh` states it plainly: *nothing secret is ever an
//! argument* — not to `ssh`, not to a child process. Secrets travel on stdin or
//! on an authenticated channel, never in `argv` (world-readable in `/proc`),
//! never in an inherited environment (leaks to every child, appears in crash
//! dumps and process listings).
//!
//! # Where the nsec lives at rest
//!
//! `~/.local/share/buzz/identity/<pubkey8>.ncryptsec` — NIP-49
//! scrypt-encrypted, `0600` in a `0700` directory. [D-8] makes this
//! deliberately the *same format the desktop's encrypted-backup flow already
//! produces* (`create_ncryptsec_backup` / `verify_ncryptsec_backup`,
//! `desktop/src-tauri/src/key_backup.rs`), so an operator's existing desktop
//! backup file is a directly importable TUI identity with no conversion step
//! and no second format to maintain.
//!
//! # How the daemon gets it
//!
//! **The raw nsec never crosses a process boundary — only the passphrase does.**
//! Three paths, and no fourth ([`IntakePath`]).
//!
//! An earlier draft offered `{nsec}` on `POST /session/identity` and called the
//! ncryptsec form "preferred". A preference is not a boundary: the `{nsec}` form
//! puts raw key material in the TUI's JS heap, where Bun cannot zeroize it, and
//! where any HTTP debug logging added later would capture it. It is deleted.
//! §6.4's CI boundary check asserts `{nsec}` is absent from the generated
//! client.
//!
//! # Keyless is a visible state
//!
//! A daemon running without an identity reports `archiving: false` on
//! `GET /health`, and every attached TUI renders it in the status bar as a loss
//! state. A keyless daemon must never look identical to a healthy one (§1.3
//! property 3).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{DaemonError, Result};

/// Environment variables the daemon reads **only in order to refuse them**.
///
/// §2.5: there is no environment-variable path. `buzz-cli` accepts
/// `BUZZ_PRIVATE_KEY` because it is a one-shot process invoked by a harness
/// that controls its environment; a long-lived daemon that spawns provider
/// subprocesses is a different threat model. No opt-in flag, no dev-only escape
/// hatch — a special case here is exactly the kind of parallel path that ends
/// up being the one everyone uses.
pub const REFUSED_ENV_VARS: &[&str] = &["BUZZ_PRIVATE_KEY", "BUZZ_NSEC"];

/// The three — and only three — ways key material reaches the daemon (§2.5).
#[derive(Debug, Clone)]
pub enum IntakePath {
    /// Spawn-time: `--identity-ncryptsec <path> --passphrase-stdin`. The path
    /// is on argv (it is not a secret) and the passphrase arrives on stdin, one
    /// line, then stdin closes. This is the auto-spawn path (§2.3) and the
    /// default.
    SpawnStdin {
        /// Path to the NIP-49 blob.
        ncryptsec_path: PathBuf,
    },
    /// `POST /session/identity` over the UDS. Body is **exactly**
    /// `{ncryptsec_path, passphrase}` — there is no `{nsec}` form. Used by a
    /// second client attaching to a daemon whose session was dropped, and by
    /// `buzz-tui daemon login`.
    SessionEndpoint {
        /// Path to the NIP-49 blob.
        ncryptsec_path: PathBuf,
    },
    /// `--identity-credential <name>` (or `--passphrase-file <path>`), reading
    /// the passphrase from `systemd-creds` or from a `0600` file whose
    /// directory is not group- or world-writable.
    ///
    /// This is the **unattended** path and it exists because the VPS install
    /// (§6.5) is the flagship one: a systemd-launched daemon after a 03:00
    /// reboot has no TTY and no attached client, so with only the first two
    /// paths it would run keyless — decrypting nothing and archiving nothing
    /// until a human attached a TUI and typed a passphrase. That is the
    /// always-on observer archive silently failing in exactly the unattended
    /// case it exists for. The secret is still never in argv and never in an
    /// inherited environment.
    Credential {
        /// Path to the NIP-49 blob.
        ncryptsec_path: PathBuf,
        /// `systemd-creds` credential name, or a `0600` passphrase file.
        source: CredentialSource,
    },
}

/// Where the unattended path reads its passphrase from (§2.5 path 3).
#[derive(Debug, Clone)]
pub enum CredentialSource {
    /// `systemd-creds` credential name.
    SystemdCreds(String),
    /// A `0600` file whose directory is not group- or world-writable.
    File(PathBuf),
}

/// Request body for `POST /session/identity` (§2.5 path 2).
///
/// There is deliberately **no** `nsec` variant. §6.4's boundary check asserts
/// the generated TS client never grows one; a regenerated client that does is a
/// spec regression, not a convenience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIdentityRequest {
    /// Path to the NIP-49 blob on the daemon's filesystem.
    pub ncryptsec_path: String,
    /// Passphrase. Held in a zeroizing buffer for the duration of the decrypt.
    pub passphrase: String,
}

/// A loaded identity. The secret key is held in a zeroizing wrapper (§2.5, "In
/// memory") and never leaves this crate.
///
/// TODO(wave1, §4.1.1 deliverable 2): construct this from the real
/// [`nostr::nips::nip49::EncryptedSecretKey`] decrypt. The decrypt must run
/// **off the async runtime** (`spawn_blocking`, as the desktop does) — scrypt at
/// `BACKUP_LOG_N = 18` on a tokio worker would wedge every other socket client
/// for its duration — and must read log-n **from the ncryptsec header**, never
/// assuming a NIP-49 default, because the desktop's backup uses a repo-chosen
/// `BACKUP_LOG_N` via `create_backup_with_log_n` and a hardcoded assumption
/// fails on real desktop backups.
pub struct Identity {
    /// Lowercase-hex pubkey derived from the decrypted secret.
    pub pubkey: String,
    /// The NIP-OA auth tag bound to this identity, when one is configured.
    pub auth_tag: Option<AuthTag>,
    /// Zeroizing secret-key bytes.
    secret: Zeroizing<Vec<u8>>,
}

/// Hand-written so the secret **cannot** reach a log line.
///
/// `#[derive(Debug)]` would print the key bytes: `Zeroizing<T>` forwards its
/// `Debug` to the inner `Vec<u8>`, so a single `tracing::debug!(?identity)`
/// would put raw key material in the daemon's log — defeating every other
/// control in §2.5, whose whole premise is that the secret never crosses a
/// process boundary. The redactor of [`crate::redact`] would not save us
/// either: it matches bech32 prefixes, and this is a byte array.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("pubkey", &self.pubkey)
            .field("auth_tag", &self.auth_tag)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl Identity {
    /// Wrap already-decrypted key material.
    ///
    /// Takes the secret **by zeroizing buffer**, not by `Vec<u8>`, so the
    /// caller cannot accidentally leave a plain copy behind on the way in
    /// (§2.5, "In memory").
    pub fn new(pubkey: String, secret: Zeroizing<Vec<u8>>, auth_tag: Option<AuthTag>) -> Self {
        Self {
            pubkey,
            auth_tag,
            secret,
        }
    }

    /// Borrow the secret-key bytes.
    ///
    /// Crate-internal on purpose: the secret reaches the signing path
    /// ([`SigningMode`]) and NIP-44 observer decrypt
    /// ([`crate::observer`]) and nothing else. It is deliberately not `pub` —
    /// §2.5's boundary is that key material never crosses a process edge, and
    /// a public accessor invites a caller outside this crate.
    pub(crate) fn secret_bytes(&self) -> &[u8] {
        &self.secret
    }
}

/// A NIP-OA auth tag: identity, not decoration, and it expires (§2.5).
///
/// Every write path in this codebase threads one: `BuzzClient::sign_event`
/// hard-fails when the auth-tag count does not match the configured tag,
/// `with_auth_tag` sets `x-auth-tag` on every HTTP bridge call, and
/// `connect_authenticated(url, &Keys, auth_tag)` carries it into the NIP-42
/// kind-22242 event. Without a story here, [D-2]'s send path is rejected by
/// `sign_event`'s own enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTag {
    /// The raw tag value as it goes on the wire.
    pub raw: String,
    /// Owner pubkey attested by the tag. Part of the socket-path preimage
    /// (§2.2) — see [`crate::config::SocketIdentity`].
    pub owner_pubkey: String,
    /// Unix seconds at which the tag stops being valid.
    pub expires_at: i64,
}

impl AuthTag {
    /// Whether the tag has expired as of `now` (unix seconds).
    ///
    /// §2.5: expiry is detected **proactively**, from the tag's `created_at<t` /
    /// `created_at>t` conditions, so `auth_failed{reason: "oa_expired"}` fires
    /// *before* the relay rejects — §2.6 already promises that UX.
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// Which signing entry point a write uses (§2.5, "Two signing entry points").
///
/// Mirrors the CLI. The unchecked variant exists because NIP-IA 9035/9036 must
/// *not* carry the ambient tag — a daemon with one signing path gets that
/// wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningMode {
    /// Asserts exactly one auth tag, ported verbatim from `sign_event`.
    Checked,
    /// No ambient auth tag. Used for NIP-IA 9035/9036 only.
    Unchecked,
}

/// Refuse any secret-bearing environment variable (§2.5).
///
/// Called once at startup, before any listener binds. The daemon reads these
/// variables only to refuse them, with a message pointing at
/// `--passphrase-stdin`.
pub fn refuse_env_key_paths() -> Result<()> {
    for var in REFUSED_ENV_VARS {
        if std::env::var_os(var).is_some() {
            return Err(DaemonError::EnvKeyRefused { var });
        }
    }
    Ok(())
}

/// Read exactly one passphrase line from stdin, then close it (§2.5 path 1).
///
/// The returned buffer zeroizes on drop. The trailing newline is stripped; no
/// other trimming happens, because a passphrase may legitimately begin or end
/// with a space.
pub fn read_passphrase_from_stdin() -> Result<Zeroizing<String>> {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Zeroizing::new(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.5: the daemon reads `BUZZ_PRIVATE_KEY` only to refuse it.
    #[test]
    fn env_var_names_include_the_cli_variable() {
        assert!(REFUSED_ENV_VARS.contains(&"BUZZ_PRIVATE_KEY"));
    }

    #[test]
    fn refusal_carries_its_own_code() {
        let err = DaemonError::EnvKeyRefused {
            var: "BUZZ_PRIVATE_KEY",
        };
        assert_eq!(err.code(), "env_key_refused");
        assert!(err.to_string().contains("--passphrase-stdin"), "{err}");
    }

    /// §2.5: expiry is proactive, so the boundary is `now >= expires_at`.
    #[test]
    fn auth_tag_expiry_is_inclusive_of_the_boundary() {
        let tag = AuthTag {
            raw: "tag".into(),
            owner_pubkey: "aa".repeat(32),
            expires_at: 1_000,
        };
        assert!(!tag.is_expired(999));
        assert!(tag.is_expired(1_000));
        assert!(tag.is_expired(1_001));
    }

    /// §2.5: the secret is held in a zeroizing buffer and is reachable only
    /// from inside this crate.
    #[test]
    fn secret_is_held_zeroizing_and_crate_internal() {
        let identity = Identity::new(
            "aa".repeat(32),
            Zeroizing::new(vec![1u8, 2, 3]),
            Some(AuthTag {
                raw: "tag".into(),
                owner_pubkey: "bb".repeat(32),
                expires_at: i64::MAX,
            }),
        );
        assert_eq!(identity.secret_bytes(), &[1, 2, 3]);
        // The Debug impl must not print key material — a daemon log line
        // carrying the secret would defeat every other control in §2.5.
        let debug = format!("{identity:?}");
        assert!(!debug.contains("[1, 2, 3]"), "{debug}");
    }

    /// §2.5/§6.4: `POST /session/identity` has no `{nsec}` form. This asserts
    /// the Rust side of the boundary; the TS side is asserted by
    /// `just tui-check-boundary`.
    #[test]
    fn session_identity_request_has_no_nsec_field() {
        let json = serde_json::to_string(&SessionIdentityRequest {
            ncryptsec_path: "/tmp/x.ncryptsec".into(),
            passphrase: "pw".into(),
        })
        .unwrap();
        assert!(!json.contains("nsec"), "{json}");
        assert!(json.contains("ncryptsec_path"), "{json}");
    }
}
