/**
 * The TUI's entire network layer — DESIGN.md §2.1, §6.4.
 *
 * The invariant that makes the front end disposable: this is **one HTTP client
 * and one line reader**. It never parses a Nostr event, never sees a key, never
 * knows a relay URL, and never learns an event kind number.
 *
 * §6.4 turns that from a rule into a CI gate (`just tui-check-boundary`), which
 * fails the build if `tui/src/` contains any Nostr/secp256k1/NIP-44/bech32
 * dependency, any bare 4-digit-and-up event-kind integer, any hand-written
 * request struct for a daemon endpoint, any decode of a pagination cursor, any
 * literal hex colour, any `{nsec}` field on the generated
 * `POST /session/identity` client, or any hardcoded key string in a handler.
 *
 * The test for whether the design succeeded: a `ratatui` front end can be
 * written against the same daemon and lose **zero** protocol work.
 *
 * TODO(wave1, §4.1.1 deliverable 14): replace the hand-written surface below
 * with the **generated** client. `just daemon-spec-check` regenerates the
 * OpenAPI document and the TS client and fails if either differs from what is
 * committed — which is what makes "adding an endpoint without adding it to the
 * spec is a build failure" real, and what catches client drift in the same
 * step. Until then this file holds only the handshake types the shell needs to
 * boot, and §6.4's "no hand-written request struct" clause is what deletes it.
 */

/**
 * Minimum daemon API version this client can talk to (§2.3 [D-1]).
 *
 * The rule is a **floor**: `daemon.api_version >= client.min_api_version`.
 * Below the floor is a hard error naming the remedy. At or above it,
 * `capabilities[]` decides which screens exist, so a later-wave TUI attached to
 * an earlier-wave daemon on a remote box *hides* what it cannot serve rather
 * than erroring inside it.
 *
 * Exact-match would be actively harmful in the install shape this product is
 * built for: on the VPS the daemon runs under `systemd --user` with
 * `--idle-timeout 0`, so "restart the daemon to match your client" means
 * killing the always-on observer archive that §6.5's install exists for.
 */
export const MIN_API_VERSION = 1;

/** `GET /health` response (§2.3). */
export interface Health {
  /** Daemon semver. */
  version: string;
  /** Wire-contract version, compared against {@link MIN_API_VERSION}. */
  api_version: number;
  /** Implemented API groups — which screens exist. */
  capabilities: string[];
  /**
   * False when the daemon is running without an identity.
   *
   * §2.5: **keyless is a visible state, not a quiet one.** Every attached TUI
   * renders this in the status bar as a loss state; a keyless daemon must never
   * look identical to a healthy one (§1.3 property 3).
   */
  archiving: boolean;
}

/** Result of the version-floor check. */
export type VersionCheck = { ok: true } | { ok: false; message: string };

/**
 * Apply the §2.3 [D-1] compatibility floor.
 *
 * The message is the one the design specifies verbatim, because "the daemon is
 * too old" without the two numbers and the remedy is a dead end (§1.3
 * property 2).
 */
export function checkApiVersion(health: Health): VersionCheck {
  if (health.api_version >= MIN_API_VERSION) return { ok: true };
  return {
    ok: false,
    message:
      `daemon ${health.version} (api ${health.api_version}) is running; ` +
      `this client needs api >= ${MIN_API_VERSION} — upgrade the daemon`,
  };
}

/**
 * Whether a capability is served by the attached daemon (§2.3 [D-1]).
 *
 * Screens gate on this rather than on a version number, so an unimplemented
 * area is *hidden* rather than erroring when the user walks into it.
 */
export function hasCapability(health: Health, capability: string): boolean {
  return health.capabilities.includes(capability);
}

/**
 * Connection state as surfaced by the daemon (§2.6).
 *
 * Carried through **verbatim**: these states look identical to "hung" if you
 * collapse them, and auth failure must be visually and textually distinct from
 * network failure, with its remediation inline.
 *
 * Rendered as **chrome, not toasts** (ux-patterns P26): a two-segment status-bar
 * indicator, one segment per link.
 */
export type ConnectionState =
  | { state: "disconnected" }
  | { state: "connecting" }
  | { state: "authenticating" }
  | { state: "connected" }
  | { state: "rate_limited"; retry_after_ms: number }
  | { state: "reconnecting"; attempt: number; next_retry_in_ms: number }
  | { state: "dns_brownout" }
  | { state: "auth_failed"; reason: string };

/**
 * Whether a connection state is a **loss** state the status bar must surface.
 *
 * §1.3 property 3: "a chat client that *looks* idle while its socket is dead is
 * the worst failure mode in this product".
 */
export function isLossState(state: ConnectionState): boolean {
  return state.state !== "connected";
}
