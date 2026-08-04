//! SQLite cache — the reason a daemon restart is invisible.
//!
//! Implements the storage half of Wave-1 daemon deliverable 3 (`DESIGN.md`
//! §4.1.1) and the durability promises of §2.3 and §2.7.
//!
//! The database lives at `~/.local/share/buzz/<hash>/cache.db`, `0600` in a
//! `0700` directory — see [`crate::config::SocketIdentity::cache_path`].
//!
//! # What it holds
//!
//! Channels, rosters, profiles, read-state, reactions, per-channel drafts, and
//! the observer archive. §2.7: reads degrade to this cache when the relay is
//! unreachable and the header shows a staleness marker — what the operator
//! loses while offline is *new* events, not their history.
//!
//! §2.3: a daemon crash reloads the cache **from SQLite, not from the relay**,
//! which is what makes §4.1.4 exit criterion 4 ("a daemon restart mid-session is
//! invisible to the TUI beyond a brief chrome state change, and loses no
//! read-state") achievable at all.
//!
//! # Observer frames are ciphertext at rest
//!
//! [D-3]: kind-24200 payloads are the richest plaintext on the box, and decrypt
//! is a pure function of (owner key, event), so storing ciphertext costs one
//! ECDH per read and nothing else. The archive's primary key is
//! [`crate::observer::FrameKey`] with an **idempotent upsert**, so even a
//! within-window replay is a no-op rather than a duplicate row.
//!
//! # Drafts live here, not in the TUI's state dir
//!
//! §4.1.2: §2.1's diagram shows two TUIs on one daemon and §1.5 calls the
//! second-client case real from day one — so a per-front-end draft store means
//! the same operator composing in `#engineering` in pane 1 and pane 2 gets two
//! silently diverging texts, and whichever sends last wins. Drafts are
//! per-*identity*, which is exactly what makes them different from frecency
//! ([D-2] keeps that client-side for the opposite reason: it is UI
//! personalization).

/// The daemon's SQLite store.
///
/// TODO(wave1, §4.1.1 deliverable 3): open the database at
/// [`crate::config::SocketIdentity::cache_path`] with `0600`, apply the schema
/// (channels, roster, profiles, read-state slots, reactions, drafts, observer
/// archive keyed by [`crate::observer::FrameKey`]), and serve the cached reads
/// of §2.7.
///
/// Deliberately **not** wired to a driver yet: the root workspace does not
/// carry `rusqlite` (only the desktop crate does, and that workspace is
/// excluded), so adding one is a dependency decision that belongs with the
/// implementation PR rather than the scaffold. The measured-cheap dep cone that
/// §6.1 relies on — `buzz-ws-client` + `buzz-sdk` at ~34 s — is what the
/// scaffold's `cargo check -p buzz-daemon` gate protects.
#[derive(Debug, Default)]
pub struct Cache {
    _private: (),
}

impl Cache {
    /// An unopened handle.
    pub fn new() -> Self {
        Self::default()
    }
}
