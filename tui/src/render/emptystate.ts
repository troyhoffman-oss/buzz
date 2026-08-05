/**
 * Empty-state rows — DESIGN.md §1.3 property 3, "loss is always visible".
 *
 * > A chat client that *looks* idle while its socket is dead is the worst
 * > failure mode in this product.
 *
 * A list with nothing in it is exactly that failure in miniature. Three very
 * different situations collapse into the same blank pane:
 *
 * 1. the community genuinely has no channels yet;
 * 2. the daemon has not reached the relay, so nothing has been ingested;
 * 3. the endpoint that would answer is not mounted by this daemon build.
 *
 * Only the first is benign, and the operator's next action differs in each
 * case — invite someone, check the connection, upgrade the daemon. Rendering
 * all three as thirty blank rows tells them nothing and, worse, reads as
 * *working*. That is the indistinguishability §1.3 property 3 forbids by name.
 *
 * `layers/search.ts` already got this right for its own list — `no results` for
 * zero hits, and a distinct `type to search` prompt for a query nobody has
 * typed yet, because "your empty search found nothing" is a false negative.
 * This module is that reasoning generalized so every list layer shares one
 * answer rather than each inventing its own (or, as measured at M2, none).
 *
 * # Why the cause comes from the client, not from the row count
 *
 * `UdsClient.missing` already records every endpoint that answered `404`
 * (`client/uds-client.ts`), and `session.connection` already carries the real
 * connection state. Both were being collected and then dropped: nothing outside
 * the client read `missing`, so the evidence that would have made an empty
 * screen legible never reached the screen. {@link emptyStateRows} takes both as
 * inputs precisely so the *cause* is rendered rather than re-derived from the
 * absence, which cannot distinguish them.
 */

import type { ConnectionState } from "../client/types";
import { pad } from "./width";

/** What a list needs to explain its own emptiness. */
export interface EmptyContext {
  /** The daemon's reported relay connection state (`GET /session`). */
  readonly connection: ConnectionState;
  /**
   * Endpoints that answered `404` — `UdsClient.missing`.
   *
   * Empty for the fixture transport, which mounts everything by construction.
   */
  readonly missing: readonly string[];
}

/**
 * Whether the daemon is actually talking to the relay.
 *
 * `connected` is the only state in which "there is nothing here" is a statement
 * about the community rather than about the transport. Everything else —
 * including `reconnecting` and `rate_limited`, which are *transient* — means
 * the list is empty because data has not arrived, and saying otherwise would be
 * a confident lie during the exact window an operator is trying to diagnose.
 */
function isLive(connection: ConnectionState): boolean {
  return connection.state === "connected";
}

/**
 * A one-line reason for the connection not being live.
 *
 * Deliberately mirrors the statusline's vocabulary rather than inventing a
 * second one: an operator who has learned `○ offline` from the chrome should
 * not have to learn a different word for the same state from a list body.
 */
function connectionReason(connection: ConnectionState): string {
  switch (connection.state) {
    case "disconnected":
      return "the daemon is not connected to the relay";
    case "connecting":
      return "connecting to the relay…";
    case "authenticating":
      return "authenticating with the relay…";
    case "reconnecting":
      return `reconnecting to the relay (attempt ${connection.attempt})…`;
    case "rate_limited":
      return "the relay is rate-limiting this client";
    case "dns_brownout":
      return "the relay's DNS is not resolving";
    case "auth_failed":
      return `relay auth failed: ${connection.reason}`;
    default:
      return "the daemon is not connected to the relay";
  }
}

/**
 * Rows for an empty list, naming *why* it is empty.
 *
 * `subject` is the plural noun the layer lists ("channels", "agents"), used in
 * the benign case only — the two failure cases are about the transport and read
 * identically whatever the layer was trying to show.
 *
 * `endpoint` is the route that would have supplied the rows. When it is in
 * `missing`, this daemon build does not serve it, and that outranks the
 * connection state: a mounted-but-disconnected daemon will fill in on its own,
 * whereas an unmounted route never will, no matter how long you wait.
 *
 * Two rows, not one: the second names the remedy, because §1.3 property 2's
 * "never a broken imitation, never silence" applies to a dead end you *arrived*
 * at just as much as to one you walked into.
 */
export function emptyStateRows(
  subject: string,
  endpoint: string,
  context: EmptyContext,
  cols: number,
): string[] {
  if (context.missing.includes(endpoint)) {
    return [
      pad(`  no ${subject} — this daemon does not serve ${endpoint}`, cols),
      pad("  the daemon is older than this client; upgrade it", cols),
    ];
  }
  if (!isLive(context.connection)) {
    return [
      pad(
        `  no ${subject} yet — ${connectionReason(context.connection)}`,
        cols,
      ),
      pad("  this list fills in once the relay connects", cols),
    ];
  }
  return [pad(`  no ${subject}`, cols)];
}
