//! Daemon error model.
//!
//! Implements `DESIGN.md` §2.4 (the endpoint error shapes) and the parts of
//! §2.3 that make **spawn failure a first-class outcome** rather than a
//! timeout: a daemon that exits immediately — wrong passphrase, corrupt
//! ncryptsec, socket in use — must surface its own redacted stderr, because
//! "timed out after 5 s" for a bad passphrase is the kind of dead end §1.3
//! property 2 forbids.
//!
//! Every variant carries a stable machine code (`DaemonError::code`) so the
//! front end renders remediation without string-matching a human message.

use std::fmt;

/// Convenience alias for daemon fallible operations.
pub type Result<T> = std::result::Result<T, DaemonError>;

/// A daemon-level failure with a stable machine code.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The runtime directory, socket path, or identity path could not be
    /// resolved or has unsafe permissions (§2.5, §6.5).
    #[error("path error: {0}")]
    Path(String),

    /// The socket's parent directory is group- or world-writable. Refusing here
    /// is what closes the `ssh -L` hole of §2.5: the forwarded socket is
    /// created with the process umask, not `0600`.
    #[error("refusing socket {path}: parent directory is group- or world-writable")]
    UnsafeSocketDir {
        /// The rejected socket path.
        path: String,
    },

    /// A connection arrived from a uid other than the daemon's own. Filesystem
    /// permissions are the authorization model; peercred is the belt to that
    /// suspenders on a box with a mis-permissioned runtime directory (§2.5).
    #[error("peer credential check failed: uid {peer_uid} != {daemon_uid}")]
    PeerCredentialRejected {
        /// uid reported by `SO_PEERCRED` / `LOCAL_PEERCRED`.
        peer_uid: u32,
        /// uid the daemon itself runs as.
        daemon_uid: u32,
    },

    /// `BUZZ_PRIVATE_KEY` (or any other secret-bearing environment variable)
    /// was set. §2.5 is explicit that there is **no** environment-variable
    /// path: the daemon reads the variable only to refuse it. No opt-in flag,
    /// no dev-only escape hatch — a special case here is exactly the kind of
    /// parallel path that ends up being the one everyone uses.
    #[error(
        "{var} is set; the daemon never reads a key from the environment — use --passphrase-stdin"
    )]
    EnvKeyRefused {
        /// The offending variable name.
        var: &'static str,
    },

    /// The ncryptsec could not be decrypted (wrong passphrase, corrupt blob, or
    /// an unreadable NIP-49 header). §2.5 [D-8] requires log-n to be read from
    /// the header rather than assumed.
    #[error("identity decrypt failed: {0}")]
    IdentityDecrypt(String),

    /// The NIP-OA auth tag is malformed or already expired. §2.5 requires this
    /// to be its own distinct error, never a generic auth failure.
    #[error("auth tag rejected: {0}")]
    AuthTag(String),

    /// The per-user daemon cap of §2.2 was reached. The message names the
    /// remedy because `/daemon/shutdown` without an enumeration path is a leak
    /// with no broom.
    #[error("daemon limit reached ({cap} live); run `buzz-tui daemon list`")]
    DaemonLimitReached {
        /// The configured hard cap.
        cap: usize,
    },

    /// A NIP-CW page failed the bounds-integrity checks of [D-10]. The page is
    /// discarded and retried; it is never partially applied.
    #[error("window page failed bounds integrity: {0}")]
    WindowIntegrity(String),

    /// A filter was about to leave the daemon without an explicit `kinds`.
    /// §2.4's global invariant: a kindless filter can match a `P_GATED_KIND`
    /// and is refused by the relay's `p_gated_filters_authorized` gate.
    #[error("refusing to emit a filter with no explicit kinds ({context})")]
    KindlessFilter {
        /// Where the filter was about to be emitted from.
        context: &'static str,
    },

    /// A cursor did not carry the `c1.` prefix of [D-6]. The prefix makes a
    /// future cursor format a clean rejection rather than a mis-parse.
    #[error("unrecognized cursor format")]
    BadCursor,

    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl DaemonError {
    /// Stable machine code for this error, as carried in the HTTP body's
    /// `code` field (§2.4). The front end switches on this, never on the
    /// human-readable message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Path(_) => "path_error",
            Self::UnsafeSocketDir { .. } => "unsafe_socket_dir",
            Self::PeerCredentialRejected { .. } => "peer_rejected",
            Self::EnvKeyRefused { .. } => "env_key_refused",
            Self::IdentityDecrypt(_) => "identity_decrypt_failed",
            Self::AuthTag(_) => "auth_tag_rejected",
            Self::DaemonLimitReached { .. } => "daemon_limit_reached",
            Self::WindowIntegrity(_) => "window_integrity",
            Self::KindlessFilter { .. } => "kindless_filter",
            Self::BadCursor => "bad_cursor",
            Self::Io(_) => "io_error",
            Self::Json(_) => "json_error",
        }
    }
}

/// The JSON body shape returned for a [`DaemonError`] (§2.4 error model).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ErrorBody {
    /// Stable machine code — see [`DaemonError::code`].
    pub code: String,
    /// Human-readable message. Always passed through the redactor of
    /// [`crate::redact`] before it leaves the process (§2.5).
    pub message: String,
}

impl From<&DaemonError> for ErrorBody {
    fn from(err: &DaemonError) -> Self {
        Self {
            code: err.code().to_string(),
            message: crate::redact::redact(&err.to_string()),
        }
    }
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
