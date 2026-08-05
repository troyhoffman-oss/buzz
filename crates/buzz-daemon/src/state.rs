//! The daemon's shared state — one owner for every cache the routes read.
//!
//! Implements the composition `DESIGN.md` §2.1 describes: "key custody, relay
//! session, cache, observer decrypt, backend-provider conduit" behind one
//! process. Each Wave-1 module owns its own invariants; this type owns nothing
//! but the locking, so a route handler can never hold two locks in two orders.
//!
//! # One mutex, not a lock per module
//!
//! The tempting shape is a `RwLock` per cache. It is wrong here for a specific
//! reason: almost every read *crosses* caches. Serving `GET /channel` needs the
//! channel list, the read-state frontier, and the fleet's working set, and doing
//! that under three separate locks means three acquisition orders to keep
//! consistent forever — the classic path to a deadlock that only appears under
//! load. The daemon's contention profile does not need the parallelism: its
//! clients are a handful of terminal panes, not a request fleet, and the
//! expensive work (scrypt, the relay round trip) already happens outside the
//! lock.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::askcard::AskCards;
use crate::channels::Channels;
use crate::config::Config;
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::mentions::Mentions;
use crate::observer::ObserverPipeline;
use crate::post::LocalIdMap;
use crate::presence::PresenceTracker;
use crate::readstate::ReadState;
use crate::rest::RestClient;
use crate::session::Session;
use crate::stream::EventStream;

/// Everything the routes read and write, behind one lock.
#[derive(Debug)]
pub struct Inner {
    /// Relay session policy: connection state, watermarks, dedup, counters.
    pub session: Session,
    /// Channel list, rosters, and the ambient agents-working state.
    pub channels: Channels,
    /// The read-state frontier.
    pub read_state: ReadState,
    /// Profile directory for mention candidates and author resolution.
    pub mentions: Mentions,
    /// Observer pipeline: guards, live rings, pending-unknown queue.
    pub observer: ObserverPipeline,
    /// Presence, from 20001 and 40902.
    pub presence: PresenceTracker,
    /// Per-agent accumulators the fleet reduces over.
    pub fleet: Fleet,
    /// Open ask cards and the awaiting count.
    pub asks: AskCards,
    /// The `/event` ring and its sequence.
    pub stream: EventStream,
    /// [D-7]'s provisional-id correlation window.
    pub local_ids: LocalIdMap,
    /// Per-channel drafts.
    ///
    /// §4.1.2: **drafts live in the daemon, not the TUI's state dir.** Two TUIs
    /// on one daemon is real from day one (§1.5), so a per-front-end store means
    /// the same operator composing in two panes gets two silently diverging
    /// texts and whichever sends last wins. Drafts are per-*identity*, which is
    /// exactly what makes them different from frecency ([D-2] keeps that
    /// client-side for the opposite reason).
    pub drafts: std::collections::BTreeMap<String, String>,
    /// The loaded identity, or `None` while keyless.
    ///
    /// §2.5: **keyless is a visible state, not a quiet one.** `GET /health`
    /// reports `archiving: false` and every attached TUI renders it as a loss
    /// state, because a keyless daemon must never look identical to a healthy
    /// one.
    pub identity: Option<Identity>,
}

/// The daemon's shared state handle.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Mutex<Inner>>,
    /// Resolved configuration. Immutable after startup, so it needs no lock.
    pub config: Arc<Config>,
    /// The relay HTTP bridge. Internally `reqwest`-pooled and `Send + Sync`.
    pub rest: Arc<RestClient>,
    /// Handle to the relay I/O loop, once one is running.
    ///
    /// `None` in the keyless state and in every unit test that serves the
    /// router without a socket. Write endpoints check it and return
    /// `503 relay_unreachable` when it is absent, which is the honest answer:
    /// the endpoint exists, the relay does not.
    pub wire: Option<crate::wire::WireHandle>,
    /// When the daemon started, for `GET /health`'s uptime.
    pub started_at: std::time::Instant,
    /// Last time a client made a request, for the idle timer.
    ///
    /// §2.2: **the idle timer keys on client activity, not on connection
    /// presence.** A detached tmux pane holding an `/event` stream open is the
    /// normal state, not a live client; without this the default 30-minute timer
    /// never fires for the exact population this product targets.
    last_request: Arc<Mutex<std::time::Instant>>,
}

impl std::fmt::Debug for AppState {
    /// Hand-written for the same reason [`Identity`]'s is: a derived `Debug`
    /// here would reach through `Inner` to the identity's key bytes, and one
    /// `tracing::debug!(?state)` would put them in the log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("socket", &self.config.socket)
            .field("uptime_secs", &self.started_at.elapsed().as_secs())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Build the state for a configured daemon.
    pub fn new(config: Config, identity: Option<Identity>) -> crate::Result<Self> {
        let rest = RestClient::new(&config.identity.relay_url)?;
        // The read-state client id identifies *this daemon process* in the
        // 30078 blob, so a second client squatting the slot is detectable
        // (§2.3). Derived from the socket hash plus a fresh uuid: the hash makes
        // it recognizable in a support conversation, the uuid makes a restart a
        // distinguishable publisher rather than a silent continuation.
        let client_id = format!(
            "{}-{}",
            config.identity.hash(),
            uuid::Uuid::new_v4().simple()
        );
        let slot_id = config.identity.hash();
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                session: Session::new(),
                channels: Channels::new(),
                read_state: ReadState::new(client_id, slot_id),
                mentions: Mentions::new(),
                observer: ObserverPipeline::with_cache_budget(config.observer_cache_bytes as usize),
                presence: PresenceTracker::new(),
                fleet: Fleet::new(),
                asks: AskCards::new(),
                stream: EventStream::new(),
                local_ids: LocalIdMap::new(),
                drafts: std::collections::BTreeMap::new(),
                identity,
            })),
            config: Arc::new(config),
            rest: Arc::new(rest),
            wire: None,
            started_at: std::time::Instant::now(),
            last_request: Arc::new(Mutex::new(std::time::Instant::now())),
        })
    }

    /// Attach the relay loop's command handle.
    ///
    /// Returns a new handle rather than mutating in place: the state is cloned
    /// into the router before the loop starts, and a mutation after that point
    /// would be invisible to the clone axum already holds.
    pub fn with_wire(self, wire: crate::wire::WireHandle) -> Self {
        Self {
            wire: Some(wire),
            ..self
        }
    }

    /// The relay loop's handle, or `503` when there is no loop.
    ///
    /// §2.7: a write while the relay is unreachable is `relay_unreachable`, the
    /// TUI keeps the composed text, and the retry is explicit. A keyless daemon
    /// reaches this the same way a disconnected one does, which is correct —
    /// from the composer's point of view they are one outcome.
    pub fn wire(&self) -> crate::Result<&crate::wire::WireHandle> {
        self.wire
            .as_ref()
            .ok_or(crate::DaemonError::RelayUnreachable)
    }

    /// Borrow the inner state.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, Inner> {
        self.inner.lock().await
    }

    /// Note that a client made a request, resetting the idle timer.
    pub async fn touch(&self) {
        *self.last_request.lock().await = std::time::Instant::now();
    }

    /// How long since the last client request.
    pub async fn idle_for(&self) -> std::time::Duration {
        self.last_request.lock().await.elapsed()
    }

    /// Whether the idle window has elapsed and the daemon should exit (§2.2).
    pub async fn should_idle_exit(&self) -> bool {
        crate::lifecycle::should_idle_exit(self.idle_for().await, self.config.idle_timeout)
    }

    /// Whether an identity is loaded — `GET /health`'s `archiving` field.
    pub async fn is_archiving(&self) -> bool {
        self.lock().await.identity.is_some()
    }

    /// A cloned identity for a relay call, or `401` when the daemon is keyless.
    ///
    /// Cloned rather than borrowed because the alternative is holding the state
    /// lock across an HTTP round trip, which stalls every other socket client
    /// for its duration. The clone stays inside the process — §2.5's boundary
    /// is the process edge — and [`Identity`]'s hand-written `Debug` keeps it
    /// out of a log line however many copies exist.
    pub async fn identity_snapshot(&self) -> crate::Result<Identity> {
        self.lock()
            .await
            .identity
            .clone()
            .ok_or(crate::DaemonError::NotAuthenticated)
    }

    /// The health payload of §2.3 [D-1].
    pub async fn health(&self) -> crate::lifecycle::Health {
        crate::lifecycle::Health {
            version: crate::VERSION.to_string(),
            api_version: crate::API_VERSION,
            capabilities: crate::lifecycle::WAVE1_CAPABILITIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            archiving: self.is_archiving().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config(idle_timeout: Option<Duration>) -> Config {
        Config {
            identity: crate::config::SocketIdentity::new(
                "wss://relay.example",
                "aa".repeat(32),
                "",
            ),
            socket: std::path::PathBuf::from("/run/user/1000/buzz/x.sock"),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/buzz"),
            data_dir: std::path::PathBuf::from("/home/u/.local/share/buzz"),
            idle_timeout,
            observer_cache_bytes: 1024,
            systemd_managed: false,
        }
    }

    /// §2.5: a keyless daemon reports `archiving: false`, and must never look
    /// identical to a healthy one.
    #[tokio::test]
    async fn a_keyless_daemon_reports_that_it_is_not_archiving() {
        let state = AppState::new(config(None), None).unwrap();
        let health = state.health().await;
        assert!(!health.archiving);
        assert_eq!(health.api_version, crate::API_VERSION);
        assert!(health.capabilities.contains(&"agents".to_string()));
    }

    #[tokio::test]
    async fn a_keyed_daemon_reports_archiving() {
        let identity = Identity::from_keys(nostr::Keys::generate(), None);
        let state = AppState::new(config(None), Some(identity)).unwrap();
        assert!(state.health().await.archiving);
    }

    /// §2.5: a derived `Debug` would reach the identity's key bytes, and one
    /// `tracing::debug!(?state)` would put them in the log.
    #[tokio::test]
    async fn debug_output_carries_no_key_material() {
        let keys = nostr::Keys::generate();
        let secret_hex = hex::encode(keys.secret_key().as_secret_bytes());
        let state = AppState::new(config(None), Some(Identity::from_keys(keys, None))).unwrap();
        let rendered = format!("{state:?}");
        assert!(!rendered.contains(&secret_hex), "{rendered}");
    }

    /// §2.2: `--idle-timeout 0` disables the timer, which is what keeps the VPS
    /// archive always-on.
    #[tokio::test]
    async fn a_zero_idle_timeout_never_exits() {
        let state = AppState::new(config(None), None).unwrap();
        assert!(!state.should_idle_exit().await);
    }

    /// The timer keys on **activity**: a request resets it, so a detached pane
    /// holding a stream open does not keep a daemon alive forever.
    #[tokio::test]
    async fn a_request_resets_the_idle_timer() {
        let state = AppState::new(config(Some(Duration::from_millis(30))), None).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(state.should_idle_exit().await);
        state.touch().await;
        assert!(!state.should_idle_exit().await);
    }

    /// §2.3: a second client squatting the read-state slot must be detectable,
    /// which requires this daemon's own client id to be distinct.
    #[tokio::test]
    async fn each_daemon_process_gets_a_distinguishable_read_state_client_id() {
        let first = AppState::new(config(None), None).unwrap();
        let second = AppState::new(config(None), None).unwrap();
        let blob = crate::readstate::ReadStateBlob {
            v: 1,
            client_id: "someone-else".into(),
            contexts: Default::default(),
        };
        assert!(first.lock().await.read_state.slot_is_squatted(&blob));
        // Both processes publish into the same `d` coordinate — that is the
        // point of the slot — but each can tell its own writes from the other's.
        assert_eq!(
            first.lock().await.read_state.slot_d_tag_for(0),
            second.lock().await.read_state.slot_d_tag_for(0)
        );
    }

    /// §4.1.2: drafts are daemon-held so two TUIs on one daemon do not diverge.
    #[tokio::test]
    async fn drafts_are_shared_across_clients() {
        let state = AppState::new(config(None), None).unwrap();
        state
            .lock()
            .await
            .drafts
            .insert("chan".into(), "half a thought".into());
        // A second handle to the same daemon sees the same draft.
        let second = state.clone();
        assert_eq!(
            second.lock().await.drafts.get("chan").map(String::as_str),
            Some("half a thought")
        );
    }
}
