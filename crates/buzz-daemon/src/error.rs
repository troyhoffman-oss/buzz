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

    /// The relay answered with a non-success status, or the request never
    /// reached it (`status: 0`). §2.4 maps these onto the CLI's exit-code
    /// taxonomy: `502`/`503` for network and relay failures.
    #[error("relay error (status {status}): {body}")]
    Relay {
        /// HTTP status, or `0` when the request never left the process.
        status: u16,
        /// Response body, always redacted before it leaves the daemon.
        body: String,
    },

    /// The relay is unreachable and a write was attempted (§2.7). Distinct
    /// from [`Self::Relay`] because the TUI's remedy is different: it keeps the
    /// composed text in the composer and binds an explicit retry, rather than
    /// showing a relay-supplied message.
    #[error("relay unreachable")]
    RelayUnreachable,

    /// The relay's rate-limit gate is armed. Carries the countdown the composer
    /// renders instead of a spinner (§2.7).
    #[error("rate limited; retry in {retry_after_ms} ms")]
    RateLimited {
        /// Milliseconds until the gate is expected to disarm.
        retry_after_ms: u64,
    },

    /// A non-idempotent write's outcome could not be observed (§2.7).
    ///
    /// Moderation kinds 9040–9044 execute at the relay **before** dedup, so a
    /// blind resend can duplicate the mutation. This renders as "unknown —
    /// check the audit log", never as an automatic retry.
    #[error("delivery outcome unknown; check the audit log")]
    DeliveryUnknown,

    /// No identity is loaded, so a signed operation cannot proceed.
    ///
    /// Distinct from an auth *failure*: §2.5 makes keyless a visible state, and
    /// a keyless daemon answering "authentication failed" would send the
    /// operator looking for a credential problem that does not exist.
    #[error("no identity is loaded; the daemon is running keyless")]
    NotAuthenticated,

    /// More `p` tags than the SDK's build-time cap allows (§2.4).
    #[error("too many mentions: {requested} requested, cap is {cap}")]
    TooManyMentions {
        /// The SDK's cap.
        cap: usize,
        /// How many the client asked for.
        requested: usize,
    },

    /// A `from:` name matched more than one identity (§3.5). Never resolved by
    /// picking one — a silent mix of authors is worse than an error.
    #[error("author name {query:?} matched {} identities", candidates.len())]
    AmbiguousAuthor {
        /// The name that matched more than once.
        query: String,
        /// Every candidate, so the client can disambiguate without a second
        /// round trip.
        candidates: Vec<String>,
    },

    /// The requested entity is not in the cache and the relay returned nothing.
    #[error("not found: {0}")]
    NotFound(String),

    /// A client request was malformed.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A `buzz-sdk` builder rejected the parameters before signing.
    #[error("event build failed: {0}")]
    Sdk(String),

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
            Self::Relay { .. } => "relay_error",
            Self::RelayUnreachable => "relay_unreachable",
            Self::RateLimited { .. } => "rate_limited",
            Self::DeliveryUnknown => "delivery_unknown",
            Self::NotAuthenticated => "not_authenticated",
            Self::TooManyMentions { .. } => "too_many_mentions",
            Self::AmbiguousAuthor { .. } => "ambiguous_author",
            Self::NotFound(_) => "not_found",
            Self::InvalidInput(_) => "invalid_input",
            Self::Sdk(_) => "invalid_input",
            Self::Io(_) => "io_error",
            Self::Json(_) => "json_error",
        }
    }

    /// HTTP status for this error, per §2.4's mapping of the CLI exit-code
    /// taxonomy onto the daemon's surface.
    ///
    /// | CLI exit | Meaning | Status |
    /// |---|---|---|
    /// | 1 | input error | `400` |
    /// | 2 | network / relay | `502` / `503` |
    /// | 3 | auth | `401` / `403` |
    /// | 4 | other | `500` |
    /// | 5 | write conflict | `409` |
    pub fn status(&self) -> u16 {
        match self {
            Self::InvalidInput(_)
            | Self::Sdk(_)
            | Self::TooManyMentions { .. }
            | Self::BadCursor
            | Self::KindlessFilter { .. } => 400,
            Self::NotAuthenticated | Self::AuthTag(_) | Self::IdentityDecrypt(_) => 401,
            Self::PeerCredentialRejected { .. } | Self::EnvKeyRefused { .. } => 403,
            Self::NotFound(_) => 404,
            Self::DeliveryUnknown | Self::AmbiguousAuthor { .. } => 409,
            Self::DaemonLimitReached { .. } => 429,
            Self::RateLimited { .. } | Self::RelayUnreachable => 503,
            // A relay status passes through when it is one the client can act
            // on; anything else becomes 502, because the daemon is the proxy
            // and the failure is on its upstream leg.
            Self::Relay { status, .. } => match status {
                400 | 401 | 403 | 404 | 409 | 429 => *status,
                _ => 502,
            },
            Self::WindowIntegrity(_) | Self::UnsafeSocketDir { .. } | Self::Path(_) => 500,
            Self::Io(_) | Self::Json(_) => 500,
        }
    }

    /// Milliseconds the client should wait before retrying, when the error
    /// carries a countdown (§2.7: "the composer shows a live countdown rather
    /// than a spinner").
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => Some(*retry_after_ms),
            _ => None,
        }
    }
}

/// The JSON body shape returned for a [`DaemonError`] (§2.4 error model).
///
/// One shape everywhere, per `daemon-api.md` §3.13:
/// `{error: {code, message, retry_after_ms?, detail?}}`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ErrorBody {
    /// Stable machine code — see [`DaemonError::code`].
    pub code: String,
    /// Human-readable message. Always passed through the redactor of
    /// [`crate::redact`] before it leaves the process (§2.5).
    pub message: String,
    /// Countdown for a rate-limited write, so the composer renders a live
    /// number instead of a spinner (§2.7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Structured detail the client can act on without parsing `message` — the
    /// mention cap's two numbers, an ambiguous author's candidate list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl From<&DaemonError> for ErrorBody {
    fn from(err: &DaemonError) -> Self {
        let detail = match err {
            DaemonError::TooManyMentions { cap, requested } => {
                Some(serde_json::json!({"cap": cap, "requested": requested}))
            }
            DaemonError::AmbiguousAuthor { query, candidates } => {
                Some(serde_json::json!({"query": query, "candidates": candidates}))
            }
            DaemonError::DaemonLimitReached { cap } => Some(serde_json::json!({"cap": cap})),
            _ => None,
        };
        Self {
            code: err.code().to_string(),
            message: crate::redact::redact(&err.to_string()),
            retry_after_ms: err.retry_after_ms(),
            detail,
        }
    }
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
