//! `buzz-daemon` — the process that owns **all** Buzz protocol knowledge.
//!
//! Implements the architecture of `DESIGN.md` §2: a small Rust daemon that
//! reuses [`buzz_ws_client`] and [`buzz_sdk`] verbatim, speaks HTTP/1.1 +
//! ndjson/SSE over a Unix domain socket, and holds the key material. The
//! terminal front end (`tui/`) is deliberately disposable: its entire network
//! layer is one HTTP client and one line reader. It never parses a Nostr event,
//! never sees a key, never knows a relay URL, and never learns an event kind
//! number (§6.4 turns that from a rule into a CI gate).
//!
//! # Wave 1 scope
//!
//! This crate is built to the wave, not to the whole spec (§2.4, "Wave-1
//! endpoint subset"). Every module below carries a doc comment naming the
//! `DESIGN.md` section it implements, and Wave-1 deliverables that are not yet
//! written are stubs that say so rather than silently-absent files.
//!
//! # Module map → `DESIGN.md` §4.1.1 deliverables
//!
//! | Module | Deliverable |
//! |---|---|
//! | [`session`] | 1 — session layer + constant table ([D-4]) |
//! | [`identity`] | 2 — ncryptsec, NIP-OA auth tag, zeroizing storage |
//! | [`channels`] | 3 — channel discovery + cache |
//! | [`timeline`] | 4 — NIP-CW window fetch ([D-10]) |
//! | [`readstate`] | 5 — NIP-RS hierarchical frontier |
//! | [`mentions`] | 6 — mention candidates + inbox |
//! | [`search`] | 7 — search with always-set `kinds` |
//! | [`observer`] | 8 — 24200 pipeline + the nine-guard chain |
//! | [`askcard`] | 9 — ask-card projection |
//! | [`fleet`] | 10 — fleet reduction |
//! | [`presence`] | 11 — 20001 live + 40902 durable, `unknown` ≠ `offline` |
//! | [`metric`] | 12 — NIP-AM 44200 |
//! | [`stream`] | 13 — `/event` ndjson/SSE with per-topic drop policy ([D-5]) |
//! | [`openapi`] | 14 — OpenAPI 3.1 emission + TS client generation |
//!
//! Supporting infrastructure that is not itself a numbered deliverable:
//! [`config`] (§2.2 socket-path identity), [`socket`] (§2.5 peercred),
//! [`lifecycle`] (§2.2/§2.3 registry, cap, idle timer), [`redact`] (§2.5
//! redactor superset), [`cache`] (§4.1.1-3 SQLite), [`cursor`] ([D-6]),
//! [`error`] (§2.4 error model), and [`api`] (the HTTP routes themselves).

pub mod api;
pub mod askcard;
pub mod cache;
pub mod channels;
pub mod config;
pub mod cursor;
pub mod error;
pub mod fleet;
pub mod identity;
pub mod lifecycle;
pub mod mentions;
pub mod metric;
pub mod observer;
pub mod openapi;
pub mod presence;
pub mod readstate;
pub mod redact;
pub mod search;
pub mod session;
pub mod socket;
pub mod stream;
pub mod timeline;

pub use config::{Config, SocketIdentity};
pub use error::{DaemonError, Result};

/// Crate version, surfaced on `GET /health` as `version` (§2.3).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire-contract version, surfaced on `GET /health` as `api_version` (§2.3).
///
/// The compatibility rule is a **floor**, not an equality:
/// `daemon.api_version >= client.min_api_version`. What makes the floor safe is
/// that API changes are additive-only, enforced by `daemon-spec-check` (§6.2) —
/// a removed or narrowed endpoint fails the build. A genuine breaking change is
/// a bump here, which is precisely what the floor is for.
pub const API_VERSION: u32 = 1;
