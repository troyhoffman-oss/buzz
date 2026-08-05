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

use std::path::{Path, PathBuf};

use nostr::nips::nip49::EncryptedSecretKey;
use nostr::{EventBuilder, FromBech32, Keys, PublicKey, Tag};
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

/// Bech32 prefix of a NIP-49 encrypted secret key.
///
/// Origin: `desktop/src-tauri/src/key_backup.rs`'s `NCRYPTSEC_HRP`. Import
/// routing is case-insensitive because bech32 permits all-uppercase encodings.
pub const NCRYPTSEC_HRP: &str = "ncryptsec1";

/// Highest scrypt cost accepted when decrypting a blob from disk.
///
/// Origin: `desktop/src-tauri/src/key_backup.rs`'s `MAX_VERIFY_LOG_N`, itself
/// pinned to `BACKUP_LOG_N = 18`. NIP-49 leaves `log_n` client-selected, so an
/// uncapped decrypt lets a crafted payload request unbounded memory *before*
/// password authentication. [D-8] requires log-n to be read from the header;
/// this is the bound on what a header may ask for, not an assumed default.
pub const MAX_LOG_N: u8 = 18;

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

impl IntakePath {
    /// The NIP-49 blob this path decrypts.
    pub fn ncryptsec_path(&self) -> &Path {
        match self {
            Self::SpawnStdin { ncryptsec_path }
            | Self::SessionEndpoint { ncryptsec_path }
            | Self::Credential { ncryptsec_path, .. } => ncryptsec_path,
        }
    }
}

/// Where the unattended path reads its passphrase from (§2.5 path 3).
#[derive(Debug, Clone)]
pub enum CredentialSource {
    /// `systemd-creds` credential name. Resolved from
    /// `$CREDENTIALS_DIRECTORY/<name>`, which is the directory systemd creates
    /// (mode `0700`, owned by the unit's user) when `LoadCredential=` or
    /// `LoadCredentialEncrypted=` is set. Reading that file is the documented
    /// consumption interface; shelling out to `systemd-creds` would put the
    /// plaintext on another process's stdout.
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
pub struct Identity {
    /// Lowercase-hex pubkey derived from the decrypted secret.
    pub pubkey: String,
    /// The NIP-OA auth tag bound to this identity, when one is configured.
    pub auth_tag: Option<AuthTag>,
    /// Zeroizing secret-key bytes.
    secret: Zeroizing<Vec<u8>>,
    /// Reconstructed signing keys.
    ///
    /// `nostr::Keys` is not `Zeroize`, so the zeroizing buffer above stays the
    /// canonical storage; this is the handle the signing and NIP-44 paths need,
    /// rebuilt from those bytes rather than kept as a second independently
    /// sourced secret.
    keys: Option<Keys>,
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
        let keys = nostr::SecretKey::from_slice(&secret).ok().map(Keys::new);
        Self {
            pubkey,
            auth_tag,
            secret,
            keys,
        }
    }

    /// Build from a decrypted [`Keys`], the shape the NIP-49 path produces.
    pub fn from_keys(keys: Keys, auth_tag: Option<AuthTag>) -> Self {
        let pubkey = keys.public_key().to_hex();
        let secret = Zeroizing::new(keys.secret_key().as_secret_bytes().to_vec());
        Self {
            pubkey,
            auth_tag,
            secret,
            keys: Some(keys),
        }
    }

    /// Borrow the secret-key bytes.
    ///
    /// Crate-internal on purpose: the secret reaches the signing path
    /// ([`SigningMode`]) and NIP-44 observer decrypt ([`crate::observer`]) and
    /// nothing else. It is deliberately not `pub` — §2.5's boundary is that key
    /// material never crosses a process edge, and a public accessor invites a
    /// caller outside this crate.
    pub(crate) fn secret_bytes(&self) -> &[u8] {
        &self.secret
    }

    /// Borrow the signing keys. Crate-internal for the same reason as
    /// [`Self::secret_bytes`].
    pub(crate) fn keys(&self) -> Option<&Keys> {
        self.keys.as_ref()
    }

    /// The NIP-OA tag as it goes on the wire, when one is configured.
    pub fn auth_tag_nostr(&self) -> Option<Tag> {
        self.auth_tag.as_ref().and_then(|t| t.to_nostr_tag().ok())
    }

    /// Sign an event builder, injecting the NIP-OA auth tag when configured.
    ///
    /// Ported verbatim from `BuzzClient::sign_event`
    /// (`crates/buzz-cli/src/client.rs:588`), including its post-condition:
    /// after signing, the event must carry **exactly** the configured number of
    /// `auth` tags. §2.5 is explicit that without this the [D-2] send path is
    /// rejected by the CLI's own enforcement — and a daemon that silently
    /// double-tags produces relay rejections nobody can attribute.
    pub fn sign_event(&self, builder: EventBuilder) -> Result<nostr::Event> {
        let keys = self
            .keys()
            .ok_or_else(|| DaemonError::IdentityDecrypt("no identity loaded".into()))?;
        let tag = self.auth_tag_nostr();
        let builder = match tag {
            Some(ref t) => builder.tags([t.clone()]),
            None => builder,
        };
        let event = builder
            .sign_with_keys(keys)
            .map_err(|e| DaemonError::IdentityDecrypt(format!("signing failed: {e}")))?;

        let auth_count = event
            .tags
            .iter()
            .filter(|t| t.as_slice().first().map(String::as_str) == Some("auth"))
            .count();
        let expected = usize::from(tag.is_some());
        if auth_count != expected {
            return Err(DaemonError::AuthTag(format!(
                "event has {auth_count} auth tags — expected {expected}; \
                 callers must not add auth tags manually"
            )));
        }
        Ok(event)
    }

    /// Sign without the ambient NIP-OA tag and without the exactly-one check.
    ///
    /// §2.5, "Two signing entry points": this exists because NIP-IA 9035/9036
    /// must *not* carry the ambient tag — their optional `auth` tag is a
    /// content-level owner-of-agent attestation about the **target** identity,
    /// unrelated to this daemon's own membership delegation. A daemon with one
    /// signing path gets that wrong, in a way that is invisible until a relay
    /// rejects.
    pub fn sign_event_unchecked(&self, builder: EventBuilder) -> Result<nostr::Event> {
        let keys = self
            .keys()
            .ok_or_else(|| DaemonError::IdentityDecrypt("no identity loaded".into()))?;
        builder
            .sign_with_keys(keys)
            .map_err(|e| DaemonError::IdentityDecrypt(format!("signing failed: {e}")))
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
    /// The raw tag JSON as it goes on the `x-auth-tag` header.
    pub raw: String,
    /// Owner pubkey attested by the tag. Part of the socket-path preimage
    /// (§2.2) — see [`crate::config::SocketIdentity`].
    pub owner_pubkey: String,
    /// Unix seconds at which the tag stops being valid, parsed from the tag's
    /// `created_at<t` condition. `None` when the tag carries no upper bound.
    pub expires_at: Option<i64>,
    /// Unix seconds before which the tag is not yet valid, parsed from
    /// `created_at>t`. `None` when the tag carries no lower bound.
    pub not_before: Option<i64>,
}

impl AuthTag {
    /// Load and **verify** a NIP-OA tag against `agent_pubkey`.
    ///
    /// §2.5: "`verify_auth_tag` runs at load. A malformed or already-expired
    /// tag is its own distinct error, never a generic auth failure." Both
    /// failures land on [`DaemonError::AuthTag`] with a message naming which
    /// one it was, so §2.6's `auth_failed{reason}` can distinguish a malformed
    /// tag from an expired one without string-matching a relay rejection.
    pub fn load(raw: &str, agent_pubkey: &PublicKey, now: i64) -> Result<Self> {
        let owner = buzz_sdk::nip_oa::verify_auth_tag(raw, agent_pubkey)
            .map_err(|e| DaemonError::AuthTag(format!("malformed: {e}")))?;

        let conditions = serde_json::from_str::<Vec<serde_json::Value>>(raw)
            .ok()
            .and_then(|arr| arr.get(2).and_then(|c| c.as_str()).map(str::to_string))
            .unwrap_or_default();
        let (not_before, expires_at) = parse_conditions_window(&conditions);

        let tag = Self {
            raw: raw.to_string(),
            owner_pubkey: owner.to_hex(),
            expires_at,
            not_before,
        };
        if tag.is_expired(now) {
            return Err(DaemonError::AuthTag(format!(
                "expired: created_at<{} but now is {now}",
                tag.expires_at.unwrap_or_default()
            )));
        }
        Ok(tag)
    }

    /// Whether the tag has expired as of `now` (unix seconds).
    ///
    /// §2.5: expiry is detected **proactively**, from the tag's `created_at<t` /
    /// `created_at>t` conditions, so `auth_failed{reason: "oa_expired"}` fires
    /// *before* the relay rejects — §2.6 already promises that UX.
    ///
    /// A tag with no `created_at<` clause never expires; returning `true` for
    /// the unbounded case would refuse every tag minted without one, which is
    /// most of them.
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|t| now >= t)
    }

    /// Convert to the `nostr::Tag` the NIP-42 AUTH event and every signed write
    /// carry.
    pub fn to_nostr_tag(&self) -> Result<Tag> {
        buzz_sdk::nip_oa::parse_auth_tag(&self.raw)
            .map_err(|e| DaemonError::AuthTag(format!("malformed: {e}")))
    }
}

/// Parse `(not_before, expires_at)` out of a NIP-OA conditions string.
///
/// The grammar is `clause&clause&…` with `kind=`, `created_at<`, and
/// `created_at>` clauses (`crates/buzz-sdk/src/nip_oa.rs`'s `validate_clause`).
/// Unknown clauses are ignored rather than rejected: `verify_auth_tag` has
/// already validated the whole string, so anything left is a clause this
/// function does not need.
fn parse_conditions_window(conditions: &str) -> (Option<i64>, Option<i64>) {
    let mut not_before = None;
    let mut expires_at = None;
    for clause in conditions.split('&') {
        if let Some(v) = clause.strip_prefix("created_at<") {
            expires_at = v.parse::<i64>().ok();
        } else if let Some(v) = clause.strip_prefix("created_at>") {
            not_before = v.parse::<i64>().ok();
        }
    }
    (not_before, expires_at)
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

/// Read the passphrase for the unattended path (§2.5 path 3).
///
/// Both variants go through [`assert_secret_file_is_private`] first: a
/// credential the daemon reads from a world-readable file is not an unattended
/// path, it is a leak with extra steps.
pub fn read_passphrase_from_credential(source: &CredentialSource) -> Result<Zeroizing<String>> {
    let path = match source {
        CredentialSource::File(path) => path.clone(),
        CredentialSource::SystemdCreds(name) => {
            let dir = std::env::var_os("CREDENTIALS_DIRECTORY").ok_or_else(|| {
                DaemonError::Path(
                    "CREDENTIALS_DIRECTORY is not set; --identity-credential requires a \
                     systemd unit with LoadCredential="
                        .into(),
                )
            })?;
            PathBuf::from(dir).join(name)
        }
    };
    assert_secret_file_is_private(&path)?;
    let mut text = Zeroizing::new(std::fs::read_to_string(&path)?);
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    Ok(text)
}

/// Refuse a secret file that is group- or world-readable, or whose directory is
/// group- or world-writable (§2.5 path 3).
///
/// The directory check matters as much as the file's own mode: a `0600` file in
/// a `0777` directory can be replaced wholesale by any uid on the box, which
/// turns "read the operator's passphrase" into "read the attacker's".
pub fn assert_secret_file_is_private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(DaemonError::Path(format!(
                "{} is mode {:o}; a secret file must be 0600",
                path.display(),
                mode & 0o777
            )));
        }
        if let Some(dir) = path.parent() {
            let dir_mode = std::fs::metadata(dir)?.permissions().mode();
            if dir_mode & 0o022 != 0 {
                return Err(DaemonError::UnsafeSocketDir {
                    path: path.display().to_string(),
                });
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Decrypt a NIP-49 blob, reading log-n **from the header** (§2.5 [D-8]).
///
/// The desktop's backup uses a repo-chosen `BACKUP_LOG_N = 18` via
/// `create_backup_with_log_n`, not a NIP-49 default, so a hardcoded assumption
/// fails on real desktop backups. [`MAX_LOG_N`] caps what a *header* may ask
/// for, which is a different thing from assuming a value: an uncapped decrypt
/// lets a crafted blob request unbounded scrypt memory before the passphrase is
/// ever checked.
///
/// This function is **synchronous and expensive on purpose**. Callers reach it
/// through [`load_identity`], which puts it on `spawn_blocking` — scrypt at
/// log-n 18 (~256 MiB, hundreds of ms) on a tokio worker would wedge every
/// other socket client for its duration.
pub fn decrypt_ncryptsec(blob: &str, passphrase: &str) -> Result<Keys> {
    let encrypted = EncryptedSecretKey::from_bech32(blob.trim())
        .map_err(|e| DaemonError::IdentityDecrypt(format!("invalid ncryptsec: {e}")))?;
    let log_n = encrypted.log_n();
    if log_n > MAX_LOG_N {
        return Err(DaemonError::IdentityDecrypt(format!(
            "unsupported KDF cost: log_n {log_n} exceeds maximum {MAX_LOG_N}"
        )));
    }
    let secret = encrypted
        .decrypt(passphrase)
        .map_err(|_| DaemonError::IdentityDecrypt("wrong passphrase or damaged key blob".into()))?;
    Ok(Keys::new(secret))
}

/// Load an identity through one of the three intake paths (§2.5).
///
/// The scrypt decrypt runs on `spawn_blocking`; the auth tag, when present, is
/// loaded and **verified** afterwards on the async side, because
/// `verify_auth_tag` is one Schnorr verification rather than a KDF.
pub async fn load_identity(
    path: &IntakePath,
    passphrase: Zeroizing<String>,
    auth_tag_raw: Option<String>,
    now: i64,
) -> Result<Identity> {
    let ncryptsec_path = path.ncryptsec_path().to_path_buf();
    assert_secret_file_is_private(&ncryptsec_path)?;
    let blob = std::fs::read_to_string(&ncryptsec_path)?;

    let keys = tokio::task::spawn_blocking(move || decrypt_ncryptsec(blob.trim(), &passphrase))
        .await
        .map_err(|e| DaemonError::IdentityDecrypt(format!("decrypt task failed: {e}")))??;

    let auth_tag = match auth_tag_raw {
        Some(raw) => Some(AuthTag::load(&raw, &keys.public_key(), now)?),
        None => None,
    };
    Ok(Identity::from_keys(keys, auth_tag))
}

/// Characters of a pubkey used as a filename stem — §2.5's `<pubkey8>`.
pub const PUBKEY_STEM_LEN: usize = 8;

/// The `<pubkey8>` filename stem for `pubkey`, safely.
///
/// # Why this is not `&pubkey[..8]`
///
/// Because that **panics** on a non-ASCII pubkey, and a pubkey reaches these
/// functions from a JSON line on stdin — `identity set-auth-tag`'s request body
/// and `--identity` on argv — neither of which has validated it as hex yet.
/// Confirmed reachable before this fix:
///
/// ```text
/// $ echo '{"pubkey":"日本語テストです","tag":"x"}' | buzz-daemon identity set-auth-tag
/// thread 'main' panicked at identity.rs:554:
///   end byte index 8 is not a char boundary; it is inside '語'
/// ```
///
/// A panic is the wrong answer twice over. It is an unhelpful failure for a
/// typo, and §1.3 property 2 forbids the dead end — but more importantly the
/// same helper is called from the **serving** path, where a panic inside a
/// request handler is a daemon that dies holding the observer archive.
///
/// Taking characters rather than bytes also keeps the stem meaning what its
/// name says: eight *characters* of the identity, for every input.
pub fn pubkey_stem(pubkey: &str) -> String {
    pubkey.chars().take(PUBKEY_STEM_LEN).collect()
}

/// Path of the ncryptsec blob for `pubkey` (§2.5):
/// `~/.local/share/buzz/identity/<pubkey8>.ncryptsec`.
pub fn ncryptsec_path_for(identity_dir: &Path, pubkey: &str) -> PathBuf {
    identity_dir.join(format!("{}.ncryptsec", pubkey_stem(pubkey)))
}

/// Path of the NIP-OA auth tag beside the ncryptsec (§2.5).
pub fn authtag_path_for(identity_dir: &Path, pubkey: &str) -> PathBuf {
    identity_dir.join(format!("{}.authtag", pubkey_stem(pubkey)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::ToBech32;

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
            expires_at: Some(1_000),
            not_before: None,
        };
        assert!(!tag.is_expired(999));
        assert!(tag.is_expired(1_000));
        assert!(tag.is_expired(1_001));
    }

    /// An unbounded tag never expires. Treating "no `created_at<` clause" as
    /// expired would refuse every tag minted without one — including the live
    /// test identity's.
    #[test]
    fn an_unbounded_auth_tag_never_expires() {
        let tag = AuthTag {
            raw: "tag".into(),
            owner_pubkey: "aa".repeat(32),
            expires_at: None,
            not_before: None,
        };
        assert!(!tag.is_expired(i64::MAX));
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
                expires_at: Some(i64::MAX),
                not_before: None,
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

    /// §5.2's `ncryptsec round-trip` row: a blob decrypts, and **log-n is read
    /// from the header**.
    ///
    /// Deliberately built at log-n 10 rather than the desktop's 18: the
    /// property under test is "the header is authoritative", and a test that
    /// only ever sees 18 cannot distinguish reading the header from hardcoding
    /// it. The [`MAX_LOG_N`] case below covers the other side.
    #[test]
    fn ncryptsec_round_trips_reading_log_n_from_the_header() {
        use nostr::nips::nip49::KeySecurity;
        let keys = Keys::generate();
        let encrypted = EncryptedSecretKey::new(
            keys.secret_key(),
            "correct horse battery",
            10,
            KeySecurity::Unknown,
        )
        .unwrap();
        let blob = encrypted.to_bech32().unwrap();

        let recovered = decrypt_ncryptsec(&blob, "correct horse battery").unwrap();
        assert_eq!(recovered.public_key(), keys.public_key());
        assert_eq!(
            EncryptedSecretKey::from_bech32(&blob).unwrap().log_n(),
            10,
            "log-n must round-trip through the header rather than being assumed"
        );
    }

    #[test]
    fn a_wrong_passphrase_is_a_distinct_error_not_a_panic() {
        use nostr::nips::nip49::KeySecurity;
        let keys = Keys::generate();
        let blob = EncryptedSecretKey::new(keys.secret_key(), "right", 10, KeySecurity::Unknown)
            .unwrap()
            .to_bech32()
            .unwrap();
        let err = decrypt_ncryptsec(&blob, "wrong").unwrap_err();
        assert_eq!(err.code(), "identity_decrypt_failed");
    }

    /// [D-8]: the cap tracks the desktop's `MAX_VERIFY_LOG_N`. Encrypting at
    /// log-n 19 to prove the rejection would allocate ~512 MiB inside the test,
    /// which is the exact cost the guard exists to refuse — so the constant is
    /// asserted instead.
    #[test]
    fn the_log_n_cap_tracks_the_desktop_backup_cost() {
        assert_eq!(MAX_LOG_N, 18);
    }

    /// §2.5: `verify_auth_tag` runs at load, and a malformed tag is its own
    /// distinct error rather than a generic auth failure.
    #[test]
    fn a_malformed_auth_tag_is_its_own_error() {
        let agent = Keys::generate();
        let err = AuthTag::load("not json", &agent.public_key(), 0).unwrap_err();
        assert_eq!(err.code(), "auth_tag_rejected");
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    /// A genuine tag verifies, and its owner pubkey is what feeds the §2.2
    /// socket-path preimage.
    #[test]
    fn a_real_auth_tag_verifies_and_yields_its_owner() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let raw = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
        let tag = AuthTag::load(&raw, &agent.public_key(), 1_700_000_000).unwrap();
        assert_eq!(tag.owner_pubkey, owner.public_key().to_hex());
        assert!(tag.to_nostr_tag().is_ok());
    }

    /// §2.5: an already-expired tag is refused **at load**, so
    /// `auth_failed{reason: "oa_expired"}` fires before the relay rejects.
    #[test]
    fn an_expired_auth_tag_is_refused_at_load() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let raw =
            buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "created_at<1000")
                .unwrap();
        let err = AuthTag::load(&raw, &agent.public_key(), 2_000).unwrap_err();
        assert_eq!(err.code(), "auth_tag_rejected");
        assert!(err.to_string().contains("expired"), "{err}");

        // The same tag before its horizon loads fine.
        let ok = AuthTag::load(&raw, &agent.public_key(), 500).unwrap();
        assert_eq!(ok.expires_at, Some(1_000));
    }

    #[test]
    fn conditions_window_parses_both_bounds() {
        let (nb, exp) = parse_conditions_window("kind=9&created_at>100&created_at<900");
        assert_eq!(nb, Some(100));
        assert_eq!(exp, Some(900));
        assert_eq!(parse_conditions_window(""), (None, None));
    }

    /// §2.5: `sign_event` asserts exactly one auth tag, ported verbatim.
    #[test]
    fn sign_event_injects_exactly_one_auth_tag() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let raw = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
        let tag = AuthTag::load(&raw, &agent.public_key(), 0).unwrap();
        let identity = Identity::from_keys(agent, Some(tag));

        let event = identity
            .sign_event(EventBuilder::new(nostr::Kind::Custom(9), "hi"))
            .unwrap();
        let auth_tags = event
            .tags
            .iter()
            .filter(|t| t.as_slice().first().map(String::as_str) == Some("auth"))
            .count();
        assert_eq!(auth_tags, 1);
    }

    /// §2.5: the unchecked path carries **no** ambient tag — NIP-IA 9035/9036
    /// would otherwise get a membership delegation stapled to an unrelated
    /// owner attestation.
    #[test]
    fn sign_event_unchecked_omits_the_ambient_tag() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let raw = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
        let tag = AuthTag::load(&raw, &agent.public_key(), 0).unwrap();
        let identity = Identity::from_keys(agent, Some(tag));

        let event = identity
            .sign_event_unchecked(EventBuilder::new(nostr::Kind::Custom(9035), ""))
            .unwrap();
        assert!(
            !event
                .tags
                .iter()
                .any(|t| t.as_slice().first().map(String::as_str) == Some("auth")),
            "unchecked signing must not inject the ambient NIP-OA tag"
        );
    }

    /// A keyless identity signs nothing — and says so, rather than panicking.
    #[test]
    fn a_keyless_identity_refuses_to_sign() {
        let keyless = Identity::new("aa".repeat(32), Zeroizing::new(Vec::new()), None);
        let err = keyless
            .sign_event(EventBuilder::new(nostr::Kind::Custom(9), "x"))
            .unwrap_err();
        assert_eq!(err.code(), "identity_decrypt_failed");
    }

    /// §2.5 path 3: a group-readable passphrase file is refused.
    #[cfg(unix)]
    #[test]
    fn a_group_readable_passphrase_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = tmp.path().join("pw");
        std::fs::write(&file, "hunter2").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(assert_secret_file_is_private(&file).is_err());

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_secret_file_is_private(&file).unwrap();
    }

    /// §2.5 path 3: reading the credential strips the trailing newline a
    /// `systemd-creds` file carries, and nothing else — a passphrase may
    /// legitimately begin or end with a space.
    #[cfg(unix)]
    #[test]
    fn credential_read_strips_only_the_trailing_newline() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = tmp.path().join("pw");
        std::fs::write(&file, " spaced pass \n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pw = read_passphrase_from_credential(&CredentialSource::File(file)).unwrap();
        assert_eq!(&*pw, " spaced pass ");
    }

    /// §2.5: the full intake path, end to end, on `spawn_blocking`.
    #[cfg(unix)]
    #[tokio::test]
    async fn load_identity_decrypts_and_verifies_the_auth_tag() {
        use nostr::nips::nip49::KeySecurity;
        use std::os::unix::fs::PermissionsExt;

        let owner = Keys::generate();
        let agent = Keys::generate();
        let blob = EncryptedSecretKey::new(agent.secret_key(), "pw", 10, KeySecurity::Unknown)
            .unwrap()
            .to_bech32()
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = tmp.path().join("id.ncryptsec");
        std::fs::write(&path, &blob).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let raw = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
        let identity = load_identity(
            &IntakePath::SpawnStdin {
                ncryptsec_path: path,
            },
            Zeroizing::new("pw".into()),
            Some(raw),
            1_700_000_000,
        )
        .await
        .unwrap();

        assert_eq!(identity.pubkey, agent.public_key().to_hex());
        assert_eq!(
            identity.auth_tag.as_ref().unwrap().owner_pubkey,
            owner.public_key().to_hex()
        );
    }

    /// §2.5: `<pubkey8>` is the filename stem for both identity artifacts.
    #[test]
    fn identity_paths_use_the_first_eight_hex_chars() {
        let dir = Path::new("/home/u/.local/share/buzz/identity");
        let pk = "0123456789abcdef".repeat(4);
        assert_eq!(ncryptsec_path_for(dir, &pk), dir.join("01234567.ncryptsec"));
        assert_eq!(authtag_path_for(dir, &pk), dir.join("01234567.authtag"));
    }
}
