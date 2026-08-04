/**
 * The statusline — NAVIGATION.md §2.1.
 *
 * > **fixed 3 rows, truncate never wrap** [G11][G10]
 *
 * | Row | Content | Drop order under narrowing |
 * |---|---|---|
 * | 1 | relay host · identity · current scope · connection glyph | host truncates first; **connection glyph never drops** |
 * | 2 | context/attention meter (fixed 20-cell bar, atomic at every width) · unread · mentions · DMs | mentions and DMs survive; unread total goes last |
 * | 3 | live-capability row | never truncated below the affordance count |
 *
 * Row 3 is the [G13] door label, and it is the piece that carries the most
 * design weight for the least space:
 *
 * > **The same row swaps a static hint for a live count, and that count is
 * > exactly what `↓` opens.** This is where `activeWorkingByChannelId` — IA
 * > §5.3's highest-value ambient signal, given no keyboard reachability at all
 * > by the desktop (IA §6.4) — becomes a destination.
 */

import type { ConnectionState } from "../client/types";
import { toSeconds } from "../time/units";
import { displayWidth, pad, truncate } from "./width";

/** Everything the three rows read. */
export interface StatuslineState {
  readonly relayUrl: string;
  readonly identity: string;
  /** Current scope: `home`, `#engineering`, `⤷ read-state slots`, … */
  readonly scope: string;
  readonly connection: ConnectionState;
  /** False when the daemon has no identity — a visible state (§2.5). */
  readonly archiving: boolean;
  /** Attention meter fill, 0..1. */
  readonly meter: number;
  readonly unread: number;
  readonly mentions: number;
  readonly dms: number;
  readonly agentsWorking: number;
  /** Agents working in the *current* channel, when there is one. */
  readonly agentsWorkingHere?: number;
  readonly huddles: number;
  /** True on chat layers, where `↑`/`↓` are the two level-1 surfaces (§3). */
  readonly chatLayer: boolean;
}

/**
 * The connection glyph. **Never drops**, at any width (§2.1 row 1).
 *
 * Every non-connected state gets a visually distinct glyph rather than a shared
 * "not ok" marker, because §2.6 and DESIGN §1.3 property 3 both turn on the
 * same point: these states look identical to "hung" if you collapse them, and a
 * chat client that *looks* idle while its socket is dead is the worst failure
 * mode in this product.
 */
export function connectionGlyph(state: ConnectionState): string {
  switch (state.state) {
    case "connected":
      return "◉ live";
    case "connecting":
      return "◌ connecting";
    case "authenticating":
      return "◍ auth";
    case "reconnecting":
      return `◌ retry ${state.attempt}`;
    case "rate_limited":
      return `◍ limited ${toSeconds(state.retry_after_ms)}s`;
    case "dns_brownout":
      return "◍ dns";
    case "auth_failed":
      return "✖ auth failed";
    case "disconnected":
      return "○ offline";
  }
}

/** The 20-cell meter of §2.1 row 2 — **atomic at every width**. */
export const METER_CELLS = 20;

/**
 * Render the meter as a fixed 20-cell bar plus a percentage.
 *
 * "Atomic at every width" is why the bar is not responsive: a bar that shrinks
 * is a bar whose fill you cannot compare between two glances, which is the only
 * thing a meter is for. If the row cannot hold it, the row truncates around it.
 */
export function renderMeter(fill: number): string {
  const clamped = Math.max(0, Math.min(1, fill));
  const exact = clamped * METER_CELLS;
  const full = Math.floor(exact);
  const partial = exact - full;
  const PARTIALS = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"] as const;
  const partialGlyph = PARTIALS[Math.floor(partial * 8)] ?? "";
  const used = full + (partialGlyph ? 1 : 0);
  const bar =
    "█".repeat(full) +
    partialGlyph +
    "·".repeat(Math.max(0, METER_CELLS - used));
  return `[${bar}] ${Math.round(clamped * 100)}%`;
}

/**
 * The two-column indent every band in §2's frames carries.
 *
 * ```
 *   buzz://relay.example  ·  troy  ·  #engineering  ·  ◉ live
 *   [████▎···········] 41%  ·  8 unread  ·  1 mention  ·  2 DMs
 * ```
 *
 * It is not decoration: the composer's `❯ ` occupies the same two columns, so
 * the indent is what makes the focus glyph read as *in* a column rather than as
 * a character that shifted the line.
 */
const INDENT = "  ";

/**
 * Row 1: relay host · identity · scope · connection glyph — **inline**, with
 * the glyph last (§2.1's frames).
 *
 * The glyph rides the end of the same `·`-separated run rather than being
 * right-aligned into the far edge. Right-alignment would put it at a different
 * x on every resize, and "never drops" is about survival under narrowing, not
 * about pinning it to a corner: the drop loop below is what implements it.
 */
export function row1(state: StatuslineState, cols: number): string {
  const glyph = connectionGlyph(state.connection);
  // `archiving: false` is not a connection state, so it cannot ride the glyph.
  // §2.5 requires it be visible anyway: a keyless daemon that looks healthy is
  // the exact failure the state exists to prevent.
  const tail = state.archiving ? [glyph] : [glyph, "⚠ keyless"];
  const segments = [state.relayUrl, state.identity, state.scope];

  // Host truncates first (§2.1), then identity, then scope — and the glyph is
  // never in the drop list at all. Dropping whole segments rather than
  // ellipsizing each equally is deliberate: `buzz://rel…` beside a full scope
  // is more useful than three half-legible fragments.
  const room = cols - INDENT.length;
  for (let drop = 0; drop <= segments.length; drop++) {
    const line = [...segments.slice(drop), ...tail].join("  ·  ");
    if (displayWidth(line) <= room) return pad(`${INDENT}${line}`, cols);
  }
  // Below even the glyph's width, truncating it is the only option left — but
  // it is the *last* thing truncated, which is what §2.1 asks for.
  return pad(`${INDENT}${truncate(tail.join("  ·  "), room)}`, cols);
}

/** Row 2: meter · unread · mentions · DMs. Unread goes last (§2.1). */
export function row2(state: StatuslineState, cols: number): string {
  const meter = renderMeter(state.meter);
  const unread = `${state.unread} unread`;
  const mentions = `${state.mentions} mention${state.mentions === 1 ? "" : "s"}`;
  const dms = `${state.dms} DM${state.dms === 1 ? "" : "s"}`;

  // Drop order is explicit rather than emergent: mentions and DMs survive, the
  // unread total goes last. A generic right-to-left drop would take DMs first,
  // which inverts the table.
  const candidates = [
    [meter, unread, mentions, dms],
    [meter, mentions, dms],
    [mentions, dms],
    [mentions],
  ];
  const room = cols - INDENT.length;
  for (const parts of candidates) {
    const line = parts.join("  ·  ");
    if (displayWidth(line) <= room) return pad(`${INDENT}${line}`, cols);
  }
  return pad(`${INDENT}${truncate(mentions, room)}`, cols);
}

/**
 * Row 3: the [G13] door label.
 *
 * > `⏵ 3 agents working · 1 huddle · ↑↓ to navigate` when something is live,
 * > `⏵ ↑ timeline · ↓ nothing running` when nothing is.
 *
 * On a chat layer the hint names both level-1 surfaces, because that is where
 * `↑` and `↓` do something (§2.3: the drawer and message-select exist only
 * where the composer owns `❯`). On a picker layer the arrows already move the
 * list, so advertising them as doors would be a lie.
 *
 * "Never truncated below the affordance count" (§2.1) is why the last fallback
 * keeps the counts and drops the prose: the count *is* the affordance.
 */
export function row3(state: StatuslineState, cols: number): string {
  const live: string[] = [];
  const here = state.agentsWorkingHere;
  if (here !== undefined && here > 0) {
    live.push(`${here} agent${here === 1 ? "" : "s"} working here`);
  } else if (state.agentsWorking > 0) {
    live.push(
      `${state.agentsWorking} agent${state.agentsWorking === 1 ? "" : "s"} working`,
    );
  }
  if (state.huddles > 0) {
    live.push(`${state.huddles} huddle${state.huddles === 1 ? "" : "s"}`);
  }

  const hint = state.chatLayer
    ? live.length > 0
      ? "↑ timeline · ↓ live"
      : "↑ timeline · ↓ nothing running"
    : "↑↓ to navigate";

  const room = cols - INDENT.length;
  const full = `⏵ ${[...live, hint].join(" · ")}`;
  if (displayWidth(full) <= room) return pad(`${INDENT}${full}`, cols);

  const countsOnly = live.length > 0 ? `⏵ ${live.join(" · ")}` : `⏵ ${hint}`;
  if (displayWidth(countsOnly) <= room)
    return pad(`${INDENT}${countsOnly}`, cols);
  return pad(`${INDENT}${truncate(countsOnly, room)}`, cols);
}

/** The full three-row band. Always exactly three rows, at every width [G11]. */
export function renderStatusline(
  state: StatuslineState,
  cols: number,
): string[] {
  return [row1(state, cols), row2(state, cols), row3(state, cols)];
}

/** The band's height. Fixed — it is what the body's flex height subtracts. */
export const STATUSLINE_ROWS = 3;
