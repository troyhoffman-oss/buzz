//! First-run identity provisioning — the daemon half of `DESIGN.md` §2.5's
//! `buzz-tui identity import` and its create counterpart.
//!
//! §2.5 names this path explicitly and then declines to make it an endpoint:
//!
//! > If a raw-nsec import is ever needed for onboarding, it is
//! > `buzz-tui identity import` — **one shot, writes an ncryptsec to disk, then
//! > uses path 1** — not a daemon endpoint.
//!
//! # Why the *daemon* binary carries it rather than the TUI
//!
//! Because every step is protocol work, and §6.4 makes "protocol work lives in
//! `crates/buzz-daemon`" a mechanical gate rather than a preference. Generating
//! a secp256k1 keypair, decoding a bech32 `nsec`, running NIP-49 scrypt, and
//! knowing that identities live at `<pubkey8>.ncryptsec` are four pieces of
//! exactly that knowledge. A TUI that did any of them would put raw key
//! material in a JS heap Bun cannot zeroize — the same argument §2.5 uses to
//! delete the `{nsec}` request form, applied to onboarding.
//!
//! So the TUI's onboarding wizard collects text, hands it to this binary on
//! **stdin**, and reads back exactly one field: the pubkey. That is the whole
//! of its knowledge, and it is the same shape a `ratatui` front end would need.
//!
//! # What never happens here
//!
//! - **No secret on argv.** The passphrase and (for import) the secret key both
//!   arrive on stdin as one JSON line, per §2.5's "nothing secret is ever an
//!   argument".
//! - **No secret on stdout.** [`ProvisionOutcome`] carries a pubkey and a path.
//!   A generated nsec is written to the encrypted blob and dropped; it is never
//!   printed, because a caller that prints it puts it in a scrollback buffer.
//! - **No environment variable.** [`crate::identity::refuse_env_key_paths`]
//!   runs before this does.

use std::path::{Path, PathBuf};

use nostr::nips::nip49::{EncryptedSecretKey, KeySecurity};
use nostr::{FromBech32, Keys, SecretKey, ToBech32};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{DaemonError, Result};
use crate::identity::{authtag_path_for, ncryptsec_path_for, MAX_LOG_N};

/// scrypt cost for identities this daemon writes.
///
/// Pinned to the desktop's `BACKUP_LOG_N` (`key_backup.rs:23`) rather than to a
/// NIP-49 default, which is the *point* of [D-8]: a TUI-created identity and a
/// desktop backup are the same artifact at the same cost, so an operator can
/// move one to the other machine with no conversion step. Raising this later is
/// safe in one direction only — the blob self-describes its cost, so old blobs
/// keep opening, but a blob written at a cost above [`MAX_LOG_N`] would be
/// refused by our own loader. [`assert_writable_log_n`] pins that.
pub const IDENTITY_LOG_N: u8 = 18;

/// Minimum passphrase length, from the desktop's `MIN_PASSPHRASE_LEN`
/// (`key_backup.rs:51`).
///
/// A floor rather than a composition rule. scrypt at log-n 18 does the work of
/// resisting a guess; what a length floor prevents is the four-character
/// passphrase that makes the KDF irrelevant. Character-class requirements would
/// add friction without adding entropy, and the desktop does not impose them
/// either — two flows that disagree about what a valid passphrase is would make
/// a desktop backup un-importable here for a reason no message could explain.
pub const MIN_PASSPHRASE_LEN: usize = 12;

/// What to provision. Deserialized from **one JSON line on stdin**.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProvisionRequest {
    /// Mint a fresh keypair.
    Create {
        /// Passphrase for the NIP-49 blob.
        passphrase: String,
    },
    /// Import an existing key.
    ///
    /// `secret` accepts an `nsec1…`, a bare 64-char hex secret, or an
    /// `ncryptsec1…` blob **with** its own `unlock` passphrase. The third form
    /// is what makes an existing desktop backup importable directly ([D-8]);
    /// without it the operator would have to decrypt it somewhere else first,
    /// which is precisely the "somewhere else" this design is trying to avoid.
    Import {
        /// The key material, in one of the three accepted encodings.
        secret: String,
        /// Passphrase for the NIP-49 blob this writes.
        passphrase: String,
        /// Passphrase that opens `secret`, when `secret` is itself a blob.
        #[serde(default)]
        unlock: Option<String>,
    },
}

impl ProvisionRequest {
    /// The passphrase the new blob is written under.
    fn passphrase(&self) -> &str {
        match self {
            Self::Create { passphrase } | Self::Import { passphrase, .. } => passphrase,
        }
    }
}

/// What provisioning produced. **Carries no secret**, by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionOutcome {
    /// Lowercase-hex pubkey of the provisioned identity.
    pub pubkey: String,
    /// Absolute path of the NIP-49 blob that was written.
    pub ncryptsec_path: String,
    /// True when this call minted a new keypair rather than importing one.
    pub created: bool,
}

/// Reject a passphrase that cannot protect a key at rest.
///
/// Checked **before** the keypair is minted, so a refused create leaves nothing
/// on disk to clean up.
pub fn check_passphrase(passphrase: &str) -> Result<()> {
    if passphrase.chars().count() < MIN_PASSPHRASE_LEN {
        return Err(DaemonError::IdentityDecrypt(format!(
            "passphrase must be at least {MIN_PASSPHRASE_LEN} characters"
        )));
    }
    Ok(())
}

/// Assert the cost this daemon *writes* is one it can also *read*.
///
/// A compile-time-shaped invariant expressed as a runtime check because both
/// constants are `pub` and either could move independently. Writing a blob at a
/// log-n above [`MAX_LOG_N`] would produce an identity that provisioning
/// accepts and startup then refuses — a first-run flow that succeeds and leaves
/// the operator unable to launch, which is the worst place for this class of
/// mistake to surface.
pub fn assert_writable_log_n() -> Result<()> {
    if IDENTITY_LOG_N > MAX_LOG_N {
        return Err(DaemonError::IdentityDecrypt(format!(
            "cannot write identities at log_n {IDENTITY_LOG_N}: the loader caps log_n at {MAX_LOG_N}"
        )));
    }
    Ok(())
}

/// Decode key material in any of the three accepted encodings (§2.5 [D-8]).
///
/// The `ncryptsec1…` branch is what makes a desktop backup directly importable.
/// Note it reads log-n **from the header** and caps it, exactly as
/// [`crate::identity::decrypt_ncryptsec`] does: an uncapped decrypt lets a
/// crafted blob request unbounded scrypt memory before the passphrase is ever
/// checked, and that argument does not weaken because the blob arrived through
/// onboarding rather than off disk.
fn decode_secret(secret: &str, unlock: Option<&str>) -> Result<Keys> {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return Err(DaemonError::IdentityDecrypt("no key material given".into()));
    }

    // The `ncryptsec` check is case-insensitive because bech32 permits an
    // all-uppercase encoding, and a backup pasted from a QR reader may be one.
    if trimmed.to_ascii_lowercase().starts_with("ncryptsec1") {
        let Some(unlock) = unlock else {
            return Err(DaemonError::IdentityDecrypt(
                "that is an encrypted backup; its own passphrase is needed to open it".into(),
            ));
        };
        let encrypted = EncryptedSecretKey::from_bech32(trimmed)
            .map_err(|e| DaemonError::IdentityDecrypt(format!("invalid backup blob: {e}")))?;
        let log_n = encrypted.log_n();
        if log_n > MAX_LOG_N {
            return Err(DaemonError::IdentityDecrypt(format!(
                "unsupported KDF cost: log_n {log_n} exceeds maximum {MAX_LOG_N}"
            )));
        }
        let opened = encrypted.decrypt(unlock).map_err(|_| {
            DaemonError::IdentityDecrypt("wrong passphrase or damaged backup".into())
        })?;
        return Ok(Keys::new(opened));
    }

    if trimmed.to_ascii_lowercase().starts_with("nsec1") {
        let key = SecretKey::from_bech32(trimmed)
            .map_err(|e| DaemonError::IdentityDecrypt(format!("invalid nsec: {e}")))?;
        return Ok(Keys::new(key));
    }

    // Bare hex last, and only when it is exactly a 32-byte secret. Accepting a
    // shorter hex string would silently pad or truncate somebody's key.
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        let key = SecretKey::from_hex(trimmed)
            .map_err(|e| DaemonError::IdentityDecrypt(format!("invalid hex secret: {e}")))?;
        return Ok(Keys::new(key));
    }

    Err(DaemonError::IdentityDecrypt(
        "unrecognized key: expected nsec1…, ncryptsec1…, or 64 hex characters".into(),
    ))
}

/// Encrypt `keys` under `passphrase` and **verify the result before returning**.
///
/// The verification is the desktop's discipline (`create_backup_blob` decrypts
/// its own fresh blob before handing it back) and it matters more here than
/// there: this blob is not a backup of a key held somewhere else, it *is* the
/// identity. A blob that cannot be reopened is an account the operator has
/// permanently lost, discovered at next launch rather than now.
fn seal(keys: &Keys, passphrase: &str, log_n: u8) -> Result<String> {
    assert_writable_log_n()?;
    let encrypted =
        EncryptedSecretKey::new(keys.secret_key(), passphrase, log_n, KeySecurity::Unknown)
            .map_err(|e| DaemonError::IdentityDecrypt(format!("encrypt identity: {e}")))?;
    let blob = encrypted
        .to_bech32()
        .map_err(|e| DaemonError::IdentityDecrypt(format!("encode identity: {e}")))?;

    let reopened = EncryptedSecretKey::from_bech32(&blob)
        .map_err(|e| DaemonError::IdentityDecrypt(format!("verify identity: {e}")))?
        .decrypt(passphrase)
        .map_err(|e| DaemonError::IdentityDecrypt(format!("verify identity: {e}")))?;
    if Keys::new(reopened).public_key() != keys.public_key() {
        return Err(DaemonError::IdentityDecrypt(
            "verify identity: the sealed blob does not reopen to the same key".into(),
        ));
    }
    Ok(blob)
}

/// Write `contents` to `path` as `0600`, atomically, in a `0700` directory.
///
/// Atomic because the failure mode of a partial write here is a truncated
/// ncryptsec — an identity that looks present and cannot be opened. `0600` at
/// **create** rather than by a following `chmod`, for the same reason
/// [`crate::socket::bind`] sets its umask rather than chmod-ing after: the
/// window between the two is readable by every uid on a box §1.2 premises on
/// running arbitrary agents under other uids.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| DaemonError::Path(format!("{} has no parent", path.display())))?;
    crate::config::ensure_private_dir(dir)?;

    let temporary = path.with_extension("tmp");
    {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(&temporary)?;
        use std::io::Write;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Provision an identity into `identity_dir` and return its pubkey.
///
/// Refuses to overwrite an existing blob for the same pubkey: re-importing the
/// same key under a *different* passphrase would silently invalidate whatever
/// the operator wrote down, and re-importing under the same one is a no-op
/// worth naming rather than performing.
pub fn provision(request: &ProvisionRequest, identity_dir: &Path) -> Result<ProvisionOutcome> {
    provision_with_log_n(request, identity_dir, IDENTITY_LOG_N)
}

/// [`provision`] at an explicit scrypt cost.
///
/// # Why this is `pub(crate)` and not `pub`
///
/// Because a *product* caller choosing its own cost is exactly the mistake
/// [`IDENTITY_LOG_N`] exists to prevent, and [`provision`] is the only entry
/// point on the binary's path. This variant exists for **tests**, and the
/// reason is measured rather than stylistic: scrypt at log-n 18 takes ~1.4 s
/// optimized and **~105 s in a debug build**, which is what `cargo test` runs.
/// Twelve provisioning cases at that cost put the crate's suite at 476 s —
/// slow enough that the suite stops being run, which costs far more coverage
/// than a lower KDF cost in a test does.
///
/// The cost is a **parameter, not a `cfg(test)` branch**: a branch would mean
/// the production path and the tested path differ, and the one thing these
/// tests are for is proving the artifact this code writes is the artifact
/// startup can read. Every case still exercises the real NIP-49 encode,
/// decode, and bech32 round trip; only the KDF work factor changes, and
/// [`what_provisioning_writes_is_what_startup_can_read`] pins the production
/// constant separately.
pub(crate) fn provision_with_log_n(
    request: &ProvisionRequest,
    identity_dir: &Path,
    log_n: u8,
) -> Result<ProvisionOutcome> {
    check_passphrase(request.passphrase())?;

    let (keys, created) = match request {
        ProvisionRequest::Create { .. } => (Keys::generate(), true),
        ProvisionRequest::Import { secret, unlock, .. } => {
            (decode_secret(secret, unlock.as_deref())?, false)
        }
    };

    let pubkey = keys.public_key().to_hex();
    let path = ncryptsec_path_for(identity_dir, &pubkey);
    if path.exists() {
        return Err(DaemonError::IdentityDecrypt(format!(
            "an identity for {} is already provisioned at {}",
            // Through the shared helper even though `pubkey` here is derived
            // hex and provably safe to slice. One rule beats a per-site
            // audit: the next author adding a message like this copies the
            // line next to it, not the reasoning behind it.
            crate::identity::pubkey_stem(&pubkey),
            path.display()
        )));
    }

    // Zeroizing, so the encoded blob does not outlive this function in a heap
    // the allocator may hand to something else. It is ciphertext rather than
    // key material, but it is ciphertext of the whole identity and costs
    // nothing to treat carefully.
    let blob = Zeroizing::new(seal(&keys, request.passphrase(), log_n)?);
    write_private(&path, &blob)?;
    // After the blob, so a crash between the two leaves an identity that is
    // merely hard to enumerate rather than a sidecar pointing at nothing.
    write_private(&pubkey_path_for(identity_dir, &pubkey), &pubkey)?;

    Ok(ProvisionOutcome {
        pubkey,
        ncryptsec_path: path.display().to_string(),
        created,
    })
}

/// Whether an identity is already provisioned for `pubkey`.
pub fn is_provisioned(identity_dir: &Path, pubkey: &str) -> bool {
    ncryptsec_path_for(identity_dir, pubkey).exists()
}

/// Path of the public-pubkey sidecar beside an identity's blob.
///
/// # Why a sidecar exists at all
///
/// §2.5 names the blob `<pubkey8>.ncryptsec`, so the filename carries **eight**
/// characters of a pubkey the socket-path preimage needs all **sixty-four** of
/// (§2.2). NIP-49 stores ciphertext, so the missing 56 cannot be recovered from
/// the blob without the passphrase — which means without a sidecar, enumerating
/// the identity directory can tell you *that* an identity exists but never
/// *which*, and an operator who provisioned on one front end and launched
/// another would be re-onboarded into a second identity beside their first.
///
/// The sidecar is **public data**: a pubkey is what every event this identity
/// signs already broadcasts. It is written `0600` anyway, because it sits in a
/// `0700` directory whose whole posture is uniform and an exception would be
/// one more thing to reason about.
pub fn pubkey_path_for(identity_dir: &Path, pubkey: &str) -> PathBuf {
    identity_dir.join(format!("{}.pub", crate::identity::pubkey_stem(pubkey)))
}

/// Every provisioned identity in `identity_dir`, as **full pubkeys**, sorted.
///
/// Reads the `.pub` sidecars rather than the blob filenames — see
/// [`pubkey_path_for`] for why the filenames are not enough. A blob whose
/// sidecar is missing (hand-copied from another machine, or written by a build
/// predating the sidecar) is reported by its stem, because reporting nothing
/// would make the operator's identity invisible to a flow whose entire job is
/// noticing it.
pub fn provisioned_identities(identity_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(identity_dir) else {
        // A missing directory is "none provisioned", which is the first-run
        // state and not an error — returning `Result` here would make every
        // caller handle "you have not onboarded yet" as a failure.
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".ncryptsec")?;
            match std::fs::read_to_string(identity_dir.join(format!("{stem}.pub"))) {
                Ok(full) if is_pubkey(full.trim()) => Some(full.trim().to_string()),
                _ => Some(stem.to_string()),
            }
        })
        .collect();
    found.sort();
    found
}

/// Whether `value` is a lowercase-hex 32-byte pubkey.
///
/// Used to decide whether a sidecar is trustworthy. A sidecar that has been
/// truncated or edited must fall back to the stem rather than producing a
/// socket path derived from a corrupt preimage — that path would name a daemon
/// that looks right and shares nothing with the real one.
pub fn is_pubkey(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Store a NIP-OA auth tag beside an identity's blob (§2.5).
///
/// The tag is a *capability* credential rather than a secret, but it is
/// identity-bound and part of the socket-path preimage (§2.2), so it gets the
/// same `0600`-in-`0700` treatment: a tag another uid can rewrite is a tag that
/// silently changes which channels the operator can reach.
pub fn write_auth_tag(identity_dir: &Path, pubkey: &str, raw: &str) -> Result<PathBuf> {
    let path = authtag_path_for(identity_dir, pubkey);
    write_private(&path, raw.trim())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// scrypt cost for the cases below.
    ///
    /// **Measured, not chosen for taste.** At the production [`IDENTITY_LOG_N`]
    /// of 18 a single seal-and-verify is ~105 s in a debug build — which is what
    /// `cargo test` runs — and the dozen cases here took the whole crate's suite
    /// from ~5 s to **476 s**. A suite that slow stops being run, which costs
    /// more coverage than a lower work factor in a test ever could.
    ///
    /// What this does *not* weaken: every case still exercises the real NIP-49
    /// encrypt, decrypt, bech32 encode, and header-`log_n` read. Only the KDF
    /// iteration count changes, and it changes *through the same parameter the
    /// production path passes* rather than through a `cfg(test)` branch — a
    /// branch would mean the tested path and the shipped path are different
    /// code, and proving they are the same is what these tests are for.
    ///
    /// [`what_provisioning_writes_is_what_startup_can_read`] pins the production
    /// constant separately, so lowering this cannot hide a bad `IDENTITY_LOG_N`.
    const TEST_LOG_N: u8 = 8;

    /// [`provision`] at [`TEST_LOG_N`].
    fn cheap(request: &ProvisionRequest, identity_dir: &Path) -> Result<ProvisionOutcome> {
        provision_with_log_n(request, identity_dir, TEST_LOG_N)
    }

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn create_writes_a_blob_that_reopens_to_the_same_pubkey() {
        let tmp = dir();
        let outcome = cheap(
            &ProvisionRequest::Create {
                passphrase: "correct horse battery".into(),
            },
            tmp.path(),
        )
        .expect("provision");
        assert!(outcome.created);

        let blob = std::fs::read_to_string(&outcome.ncryptsec_path).expect("read blob");
        let keys =
            crate::identity::decrypt_ncryptsec(&blob, "correct horse battery").expect("reopen");
        assert_eq!(keys.public_key().to_hex(), outcome.pubkey);
    }

    /// §2.5: the blob is `0600` in a `0700` directory, and it is that at
    /// **create** rather than by a chmod afterwards.
    #[cfg(unix)]
    #[test]
    fn the_blob_and_its_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = dir();
        let nested = tmp.path().join("identity");
        let outcome = cheap(
            &ProvisionRequest::Create {
                passphrase: "correct horse battery".into(),
            },
            &nested,
        )
        .expect("provision");

        let file = std::fs::metadata(&outcome.ncryptsec_path).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o600);
        let parent = std::fs::metadata(&nested).unwrap();
        assert_eq!(parent.permissions().mode() & 0o777, 0o700);
    }

    /// The written blob must be loadable by the daemon's **own** loader, which
    /// is a different function with its own log-n cap. A cost this module can
    /// write but `identity.rs` refuses would produce a first-run flow that
    /// succeeds and then cannot launch.
    ///
    /// Asserted through [`assert_writable_log_n`] rather than by comparing the
    /// two constants inline: a direct comparison of two `const`s is a constant
    /// expression clippy (rightly) rejects, and routing it through the function
    /// is also what the *production* path calls, so this test covers the guard
    /// rather than restating its arithmetic.
    #[test]
    fn what_provisioning_writes_is_what_startup_can_read() {
        assert_writable_log_n().expect("write cost within the loader's cap");
    }

    #[test]
    fn import_accepts_an_nsec() {
        let tmp = dir();
        let source = Keys::generate();
        let nsec = source.secret_key().to_bech32().unwrap();
        let outcome = cheap(
            &ProvisionRequest::Import {
                secret: nsec,
                passphrase: "correct horse battery".into(),
                unlock: None,
            },
            tmp.path(),
        )
        .expect("provision");
        assert!(!outcome.created);
        assert_eq!(outcome.pubkey, source.public_key().to_hex());
    }

    #[test]
    fn import_accepts_bare_hex() {
        let tmp = dir();
        let source = Keys::generate();
        let outcome = cheap(
            &ProvisionRequest::Import {
                secret: source.secret_key().to_secret_hex(),
                passphrase: "correct horse battery".into(),
                unlock: None,
            },
            tmp.path(),
        )
        .expect("provision");
        assert_eq!(outcome.pubkey, source.public_key().to_hex());
    }

    /// [D-8]'s point, tested end to end: a blob written by the *desktop's* code
    /// path imports here with no conversion step. Constructed with the same
    /// `EncryptedSecretKey::new` call `key_backup.rs:60` makes, at the same
    /// cost, so this is the real artifact rather than a lookalike.
    #[test]
    fn import_accepts_a_desktop_encrypted_backup() {
        let tmp = dir();
        let source = Keys::generate();
        let backup = EncryptedSecretKey::new(
            source.secret_key(),
            "desktop backup phrase",
            TEST_LOG_N,
            KeySecurity::Unknown,
        )
        .unwrap()
        .to_bech32()
        .unwrap();

        let outcome = cheap(
            &ProvisionRequest::Import {
                secret: backup,
                passphrase: "a different tui phrase".into(),
                unlock: Some("desktop backup phrase".into()),
            },
            tmp.path(),
        )
        .expect("provision");
        assert_eq!(outcome.pubkey, source.public_key().to_hex());

        // And it is re-sealed under the *new* passphrase, not the old one.
        let blob = std::fs::read_to_string(&outcome.ncryptsec_path).unwrap();
        assert!(crate::identity::decrypt_ncryptsec(&blob, "desktop backup phrase").is_err());
        crate::identity::decrypt_ncryptsec(&blob, "a different tui phrase").expect("new phrase");
    }

    /// An encrypted backup with no unlock passphrase must say *that*, not
    /// "unrecognized key". The operator pasted the right thing.
    #[test]
    fn an_encrypted_backup_without_its_passphrase_says_so() {
        let source = Keys::generate();
        let backup = EncryptedSecretKey::new(
            source.secret_key(),
            "phrase",
            TEST_LOG_N,
            KeySecurity::Unknown,
        )
        .unwrap()
        .to_bech32()
        .unwrap();
        let err = decode_secret(&backup, None).unwrap_err();
        assert!(err.to_string().contains("its own passphrase"), "{err}");
    }

    #[test]
    fn a_wrong_unlock_passphrase_is_refused() {
        let source = Keys::generate();
        let backup = EncryptedSecretKey::new(
            source.secret_key(),
            "phrase",
            TEST_LOG_N,
            KeySecurity::Unknown,
        )
        .unwrap()
        .to_bech32()
        .unwrap();
        let err = decode_secret(&backup, Some("not the phrase")).unwrap_err();
        assert!(err.to_string().contains("wrong passphrase"), "{err}");
    }

    /// A hex string of the wrong length is refused rather than padded. Silently
    /// accepting 63 characters would provision a *different* key than the one
    /// the operator holds, and they would find out by not being able to post.
    #[test]
    fn a_truncated_hex_secret_is_refused_rather_than_padded() {
        let source = Keys::generate();
        let short = &source.secret_key().to_secret_hex()[..63];
        let err = decode_secret(short, None).unwrap_err();
        assert!(err.to_string().contains("unrecognized key"), "{err}");
    }

    #[test]
    fn a_short_passphrase_is_refused_before_anything_is_written() {
        let tmp = dir();
        let err = cheap(
            &ProvisionRequest::Create {
                passphrase: "short".into(),
            },
            tmp.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("at least"), "{err}");
        assert!(
            provisioned_identities(tmp.path()).is_empty(),
            "a refused create must leave nothing on disk"
        );
    }

    /// Re-importing the same key under a different passphrase would silently
    /// invalidate whatever the operator wrote down for the first one.
    #[test]
    fn provisioning_the_same_key_twice_is_refused() {
        let tmp = dir();
        let source = Keys::generate();
        let nsec = source.secret_key().to_bech32().unwrap();
        let request = |phrase: &str| ProvisionRequest::Import {
            secret: nsec.clone(),
            passphrase: phrase.into(),
            unlock: None,
        };
        cheap(&request("correct horse battery"), tmp.path()).expect("first");
        let err = cheap(&request("a different phrase"), tmp.path()).unwrap_err();
        assert!(err.to_string().contains("already provisioned"), "{err}");
    }

    /// The enumeration returns **full** pubkeys, which is what the socket-path
    /// preimage of §2.2 needs — the blob's filename carries only eight
    /// characters and NIP-49 is ciphertext, so without the `.pub` sidecar the
    /// remaining 56 are unrecoverable and a launcher could tell *that* an
    /// identity exists but never *which*.
    #[test]
    fn provisioned_identities_lists_full_pubkeys_and_is_empty_before_first_run() {
        let tmp = dir();
        assert!(provisioned_identities(&tmp.path().join("never-created")).is_empty());
        let outcome = cheap(
            &ProvisionRequest::Create {
                passphrase: "correct horse battery".into(),
            },
            tmp.path(),
        )
        .unwrap();
        assert_eq!(
            provisioned_identities(tmp.path()),
            vec![outcome.pubkey.clone()]
        );
        assert!(is_provisioned(tmp.path(), &outcome.pubkey));
    }

    /// A blob whose sidecar is missing — hand-copied from another machine, or
    /// written by a build predating the sidecar — is reported by its **stem**
    /// rather than dropped. Reporting nothing would make that operator's
    /// identity invisible to a flow whose whole job is noticing it, and they
    /// would be onboarded into a second identity beside their first.
    #[test]
    fn a_blob_without_a_sidecar_still_lists_by_stem() {
        let tmp = dir();
        let outcome = cheap(
            &ProvisionRequest::Create {
                passphrase: "correct horse battery".into(),
            },
            tmp.path(),
        )
        .unwrap();
        std::fs::remove_file(pubkey_path_for(tmp.path(), &outcome.pubkey)).unwrap();
        assert_eq!(
            provisioned_identities(tmp.path()),
            vec![outcome.pubkey[..8].to_string()]
        );
    }

    /// A non-ASCII pubkey must not panic.
    ///
    /// Regression, and it was a **reachable** panic rather than a theoretical
    /// one: `&pubkey[..8]` on a multibyte string is a char-boundary panic, and
    /// a pubkey arrives from a JSON line on stdin before anything has checked
    /// it is hex. Confirmed against the built binary before the fix:
    ///
    /// ```text
    /// $ echo '{"pubkey":"日本語テストです","tag":"x"}' | buzz-daemon identity set-auth-tag
    /// thread 'main' panicked: end byte index 8 is not a char boundary
    /// ```
    ///
    /// The same helpers are on the serving path, where a panic in a handler is
    /// a daemon that dies holding the observer archive.
    #[test]
    fn a_multibyte_pubkey_does_not_panic_the_path_helpers() {
        let tmp = dir();
        for pubkey in ["日本語テストです", "é", "", "🔑🔑🔑"] {
            let _ = pubkey_path_for(tmp.path(), pubkey);
            let _ = ncryptsec_path_for(tmp.path(), pubkey);
            let _ = authtag_path_for(tmp.path(), pubkey);
            assert!(!is_provisioned(tmp.path(), pubkey));
        }
        // And the write path, which is what the stdin request reaches.
        write_auth_tag(tmp.path(), "日本語テストです", "[\"auth\"]").expect("write");
    }

    /// The stem is eight **characters**, not eight bytes — so the name means
    /// the same thing for every input rather than silently shortening.
    #[test]
    fn the_stem_is_eight_characters() {
        use crate::identity::pubkey_stem;
        assert_eq!(pubkey_stem(&"a".repeat(64)), "aaaaaaaa");
        // Eight characters of a longer multibyte string — the case that used
        // to panic, since byte 8 lands inside '語'.
        assert_eq!(pubkey_stem("日本語テストですよ長い"), "日本語テストです");
        assert_eq!(pubkey_stem("short"), "short");
        assert_eq!(pubkey_stem(""), "");
    }

    /// A **corrupt** sidecar falls back to the stem rather than being trusted.
    /// A socket path derived from a truncated preimage names a daemon that
    /// looks right and shares nothing with the real one.
    #[test]
    fn a_corrupt_sidecar_is_not_trusted() {
        let tmp = dir();
        let outcome = cheap(
            &ProvisionRequest::Create {
                passphrase: "correct horse battery".into(),
            },
            tmp.path(),
        )
        .unwrap();
        std::fs::write(pubkey_path_for(tmp.path(), &outcome.pubkey), "not-a-pubkey").unwrap();
        assert_eq!(
            provisioned_identities(tmp.path()),
            vec![outcome.pubkey[..8].to_string()]
        );
    }

    /// The outcome is what crosses the process boundary to the TUI, so the
    /// assertion that matters is what it does **not** carry.
    #[test]
    fn the_outcome_carries_no_key_material() {
        let tmp = dir();
        let outcome = cheap(
            &ProvisionRequest::Create {
                passphrase: "correct horse battery".into(),
            },
            tmp.path(),
        )
        .unwrap();
        let json = serde_json::to_string(&outcome).unwrap();
        for forbidden in ["nsec", "ncryptsec1", "correct horse battery"] {
            assert!(
                !json.contains(forbidden),
                "provision outcome leaked {forbidden}: {json}"
            );
        }
    }

    #[test]
    fn the_auth_tag_lands_beside_the_blob_owner_only() {
        let tmp = dir();
        let pubkey = "ab".repeat(32);
        let path = write_auth_tag(tmp.path(), &pubkey, "[\"auth\",\"x\"]\n").expect("write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"auth\",\"x\"]");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// The request is parsed from one stdin line, so its wire form is part of
    /// the contract the TUI writes against.
    #[test]
    fn the_request_parses_from_one_json_line() {
        let create: ProvisionRequest =
            serde_json::from_str(r#"{"mode":"create","passphrase":"correct horse battery"}"#)
                .expect("create parses");
        assert!(matches!(create, ProvisionRequest::Create { .. }));

        let import: ProvisionRequest = serde_json::from_str(
            r#"{"mode":"import","secret":"nsec1x","passphrase":"p","unlock":"u"}"#,
        )
        .expect("import parses");
        match import {
            ProvisionRequest::Import { unlock, .. } => assert_eq!(unlock.as_deref(), Some("u")),
            ProvisionRequest::Create { .. } => panic!("wrong variant"),
        }

        // `unlock` is optional; the common import has none.
        let bare: ProvisionRequest =
            serde_json::from_str(r#"{"mode":"import","secret":"nsec1x","passphrase":"p"}"#)
                .expect("import without unlock parses");
        assert!(matches!(
            bare,
            ProvisionRequest::Import { unlock: None, .. }
        ));
    }
}
