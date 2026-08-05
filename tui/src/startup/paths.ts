/**
 * Where the TUI's own state lives, and where the daemon's socket is — the path
 * half of DESIGN.md §2.2 and §2.3.
 *
 * # The socket hash is the daemon's arithmetic, not ours
 *
 * §2.2 derives the socket name from
 * `sha256(relay_url + ":" + pubkey + ":" + auth_tag_owner)[0..16]`. That
 * derivation lives in `crates/buzz-daemon/src/config.rs` and this module does
 * **not** reimplement it. It cannot: `auth_tag_owner` is the owner pubkey
 * *parsed out of a NIP-OA tag*, which is protocol work §6.4 keeps out of `src/`
 * — and a second implementation that got it subtly wrong would put two clients
 * on two sockets for one identity, producing exactly the double-daemon §2.3's
 * lock exists to prevent.
 *
 * Instead the TUI **remembers** the socket path a spawn produced, keyed by
 * community, and asks the daemon to derive it otherwise. The knowledge stays in
 * one place and the client stores a string.
 */

import { homedir } from "node:os";
import { join } from "node:path";

/** `$XDG_STATE_HOME/buzz`, the TUI's own state directory. */
export function stateDir(env: NodeJS.ProcessEnv = process.env): string {
  const xdg = env.XDG_STATE_HOME;
  if (xdg && xdg.length > 0) return join(xdg, "buzz");
  return join(homedir(), ".local", "state", "buzz");
}

/**
 * The runtime directory holding sockets, locks, and pidfiles (§2.2).
 *
 * Mirrors `config::runtime_dir` on the daemon side: `$XDG_RUNTIME_DIR/buzz` on
 * Linux, `~/Library/Application Support/buzz/run` on macOS. This one *is*
 * duplicated, because the TUI has to know where to place its spawn lock before
 * a daemon exists to ask — and it is a directory name rather than a hash, so
 * the two copies cannot silently disagree about a computed value.
 */
export function runtimeDir(
  env: NodeJS.ProcessEnv = process.env,
  platform: string = process.platform,
): string {
  if (platform === "darwin") {
    return join(homedir(), "Library", "Application Support", "buzz", "run");
  }
  const xdg = env.XDG_RUNTIME_DIR;
  if (xdg && xdg.length > 0) return join(xdg, "buzz");
  // No `$XDG_RUNTIME_DIR` is normal under `su`, in a container, and in CI.
  // Falling back into the state directory keeps the daemon launchable there;
  // it is created `0700` like every other directory this file names, so the
  // §2.5 posture holds even though the location is not tmpfs.
  return join(stateDir(env), "run");
}

/** The TUI's config file — relay URL, identity, community label. */
export function configPath(env: NodeJS.ProcessEnv = process.env): string {
  return join(stateDir(env), "config.json");
}

/**
 * What the onboarding wizard writes and startup reads.
 *
 * Deliberately three fields. Everything else the TUI needs comes from the
 * daemon at attach time (`GET /session`, `GET /health`), and duplicating any of
 * it here would create a second source of truth that goes stale the first time
 * an operator changes something from another client.
 */
export interface TuiConfig {
  /** Relay websocket URL. Passed to the daemon; never parsed here. */
  readonly relayUrl: string;
  /** Pubkey of the provisioned identity, from `buzz-daemon identity provision`. */
  readonly pubkey: string;
  /** Community label for the statusline. */
  readonly communityName: string;
  /**
   * Socket path a previous spawn produced, when one has.
   *
   * Cached rather than derived — see the module docs. Absent on a fresh config,
   * which is why {@link resolveSocket} has a fallback that asks the daemon.
   */
  readonly socket?: string;
}
