//! Channel discovery and the roster cache.
//!
//! Implements Wave-1 daemon deliverable 3 (`DESIGN.md` §4.1.1): 39002
//! `#p=self` → `#d` uuids → 39000 batch, kept live from 44100/44101.
//!
//! §1.4: **multi-community aggregation stays out of the daemon.** One daemon
//! per (relay, identity); the TUI opens N and merges. Because each daemon is a
//! separate process with a separate cache and a separate relay budget, the
//! entire class of bug that the desktop's `resetCommunityState()` exists to
//! prevent — module-level caches leaking across a relay boundary — cannot occur
//! here. There is no shared memory to leak through (§2.2).

use serde::{Deserialize, Serialize};

/// Channel membership kind, queried with `#p = self` to discover channels.
pub const KIND_CHANNEL_MEMBERSHIP: u32 = 39002;

/// Channel metadata kind.
///
/// §"Common Gotchas" 1: kind **39000** for channel metadata, not 41 — kind 41 is
/// NIP-01 and unused here.
pub const KIND_CHANNEL_METADATA: u32 = 39000;

/// Member-added notification, keeping the roster live.
pub const KIND_MEMBER_ADDED: u32 = 44100;

/// Member-removed notification, keeping the roster live.
pub const KIND_MEMBER_REMOVED: u32 = 44101;

/// A cached channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    /// Channel uuid, as carried in the `#h` tag (NIP-29 group tag) rather than
    /// an `#e` tag — filters and queries scope to `h`.
    pub id: String,
    /// Display name without the leading `#`.
    pub name: String,
    /// Channel topic line.
    pub topic: Option<String>,
    /// Member count as of the last roster read.
    pub member_count: u32,
}

/// Channel discovery and cache.
///
/// TODO(wave1, §4.1.1 deliverable 3): discover via 39002 `#p=self`, collect the
/// `#d` uuids, batch-fetch 39000 metadata, and keep the roster live from
/// 44100/44101. Persist to SQLite at `~/.local/share/buzz/<hash>/cache.db`,
/// `0600` — see [`crate::cache`] and
/// [`crate::config::SocketIdentity::cache_path`].
#[derive(Debug, Default)]
pub struct Channels {
    _private: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Common Gotchas" 1: 39000, never 41.
    #[test]
    fn channel_metadata_is_39000_not_41() {
        assert_eq!(KIND_CHANNEL_METADATA, 39_000);
        assert_ne!(KIND_CHANNEL_METADATA, 41);
    }

    #[test]
    fn discovery_and_roster_kinds_match_the_design() {
        assert_eq!(KIND_CHANNEL_MEMBERSHIP, 39_002);
        assert_eq!(KIND_MEMBER_ADDED, 44_100);
        assert_eq!(KIND_MEMBER_REMOVED, 44_101);
    }
}
