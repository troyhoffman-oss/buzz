//! The HTTP surface: routes, and the Wave-1 endpoint subset.
//!
//! Implements `DESIGN.md` §2.4 and Wave-1 daemon deliverables 13–14 (§4.1.1).
//!
//! The transport is HTTP/1.1 over the Unix domain socket bound by
//! [`crate::socket::bind`]. **No TCP listener ships** (§2.5).
//!
//! # The daemon is built to the wave, not to the whole spec
//!
//! [`WAVE1_ENDPOINTS`] is the exact list from §2.4. Everything else in
//! `daemon-api.md` (backend deploy passthrough, moderation, media, DM
//! open/hide, forum, emoji sets) lands in the wave that needs it, and is absent
//! from `capabilities[]` until then ([`crate::lifecycle::WAVE1_CAPABILITIES`]).

use axum::Router;

/// The Wave-1 endpoint subset, verbatim from `DESIGN.md` §2.4.
///
/// `just daemon-spec-check` (§6.2) makes "adding an endpoint without adding it
/// to the spec is a build failure" real: the OpenAPI document is regenerated
/// and compared against the committed one, which also catches TS-client drift
/// because the generated client is committed and regenerated in the same step.
pub const WAVE1_ENDPOINTS: &[&str] = &[
    // Meta
    "/health",
    "/openapi.json",
    "/daemon",
    "/daemon/registry",
    "/daemon/shutdown",
    "/daemon/reconnect",
    // Session
    "/session",
    "/session/identity",
    "/session/relay-info",
    // Channels
    "/channel",
    "/channel/{id}",
    "/channel/{id}/member",
    "/channel/{id}/join",
    "/channel/{id}/leave",
    "/channel/{id}/message",
    "/channel/{id}/typing",
    "/channel/{id}/read",
    // Messages and threads
    "/message/{id}",
    "/message/{id}/thread",
    "/message/{id}/reaction",
    // Answer an ask card — a threaded kind:9 reply, NOT a control frame (§2.4).
    "/message/{id}/ask",
    // Search
    "/search",
    "/search/user",
    // Directory
    "/user",
    "/user/{pubkey}",
    "/mention/candidates",
    "/mention/inbox",
    "/read-state",
    "/presence",
    // Agents
    "/agent",
    "/agent/{pk}",
    "/agent/{pk}/activity",
    "/agent/{pk}/transcript",
    "/agent/{pk}/metric",
    "/agent/{pk}/control",
    "/agent/fleet",
    // The one event stream
    "/event",
];

/// Build the daemon router.
///
/// TODO(wave1): mount every route in [`WAVE1_ENDPOINTS`]. Two cross-cutting
/// layers wrap all of them:
/// 1. **Peer-credential authorization** on accept
///    ([`crate::socket::authorize_peer`]) — §2.5.
/// 2. **Response redaction** ([`crate::redact::redact`]) on every error body,
///    so a secret cannot reach a log or a client through an error path (§2.5).
pub fn router() -> Router {
    Router::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.4 lists the Wave-1 subset; duplicates would make the spec-check
    /// comparison ambiguous.
    #[test]
    fn endpoints_are_unique() {
        let unique: std::collections::BTreeSet<_> = WAVE1_ENDPOINTS.iter().collect();
        assert_eq!(unique.len(), WAVE1_ENDPOINTS.len());
    }

    /// §2.4: permission answering is `/message/{id}/ask`, not a control frame.
    #[test]
    fn ask_answering_is_a_message_endpoint() {
        assert!(WAVE1_ENDPOINTS.contains(&"/message/{id}/ask"));
    }

    /// §4.1.3: no moderation, no deploy, no media, no forum, no projects in
    /// Wave 1.
    #[test]
    fn later_wave_endpoints_are_absent() {
        for later in [
            "/moderation",
            "/backend/deploy",
            "/media",
            "/forum",
            "/project",
            "/dm/open",
        ] {
            assert!(
                !WAVE1_ENDPOINTS.iter().any(|e| e.starts_with(later)),
                "{later} is not Wave 1"
            );
        }
    }

    /// §2.3/§2.2: the meta endpoints that make the daemon supervisable.
    #[test]
    fn meta_endpoints_are_present() {
        for required in [
            "/health",
            "/openapi.json",
            "/daemon/registry",
            "/daemon/shutdown",
            "/event",
        ] {
            assert!(WAVE1_ENDPOINTS.contains(&required), "{required}");
        }
    }
}
