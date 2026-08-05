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
import {
  DEGRADED,
  FAILED,
  HINT,
  LIVE,
  MENTION,
  META,
  METER_EMPTY,
  METER_FILL,
  UNREAD,
} from "./palette";
import { type SpanStyle, type StyledRow, padRow, plain, styled } from "./span";
import { displayWidth, truncate } from "./width";

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

/**
 * The colour a connection state is drawn in.
 *
 * Three tones for eight states, deliberately: the glyphs already distinguish
 * all eight (§2.6 and DESIGN §1.3 property 3 both turn on their being
 * distinguishable), so colour's job here is the coarser question an operator
 * asks from across the room — *is it fine, is it working on it, or is it
 * broken?* Eight colours would answer a question nobody asks and would spend
 * the entire palette on one segment of one row.
 *
 * `connected` is the only green. Everything transient is amber, everything
 * terminal is red — and `auth_failed` is red rather than amber precisely
 * because §2.6 requires it be distinct from a network problem: it will not
 * resolve itself by waiting, which is exactly what amber would imply.
 */
export function connectionStyle(state: ConnectionState): SpanStyle {
  switch (state.state) {
    case "connected":
      return LIVE;
    case "auth_failed":
      return FAILED;
    case "disconnected":
      return FAILED;
    default:
      return DEGRADED;
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
  const parts = meterParts(fill);
  return `[${parts.filled}${parts.empty}] ${parts.percent}`;
}

/**
 * The meter's three pieces, so the bar can be drawn in two colours.
 *
 * Split out rather than styled by matching the finished string, for the reason
 * `render/span.ts` gives at length: the renderer knows which cells are filled
 * and a matcher would have to re-derive it from the glyphs — and `·` is a
 * legitimate character in the percentage's neighbourhood. Deriving it once,
 * here, is what keeps the string and styled forms from ever disagreeing.
 */
export function meterParts(fill: number): {
  filled: string;
  empty: string;
  percent: string;
} {
  const clamped = Math.max(0, Math.min(1, fill));
  const exact = clamped * METER_CELLS;
  const full = Math.floor(exact);
  const partial = exact - full;
  const PARTIALS = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"] as const;
  const partialGlyph = PARTIALS[Math.floor(partial * 8)] ?? "";
  const used = full + (partialGlyph ? 1 : 0);
  return {
    filled: "█".repeat(full) + partialGlyph,
    empty: "·".repeat(Math.max(0, METER_CELLS - used)),
    percent: `${Math.round(clamped * 100)}%`,
  };
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
export function row1(state: StatuslineState, cols: number): StyledRow {
  const glyph = connectionGlyph(state.connection);
  // `archiving: false` is not a connection state, so it cannot ride the glyph.
  // §2.5 requires it be visible anyway: a keyless daemon that looks healthy is
  // the exact failure the state exists to prevent.
  const tail = state.archiving ? [glyph] : [glyph, "⚠ keyless"];
  const segments = [state.relayUrl, state.identity, state.scope];

  // The **identity of who you are and where you are** is what this row is for,
  // and the M3 capture drew all of it — a 41-character relay URL included — at
  // one weight, so the eye had nowhere to land. The relay host is the least
  // interesting thing on the row (it does not change) and recedes; identity and
  // scope stay at base text; the connection glyph carries its state colour.
  // Every field of `SpanStyle` is optional, so `{}` *is* a style — the one
  // meaning "base text, inherit the frame's foreground". Saying it that way
  // rather than as a cast keeps the no-token case a first-class choice instead
  // of looking like a gap someone forgot to fill.
  const style = (index: number): SpanStyle => (index === 0 ? META : {});

  const join = (parts: readonly string[], from: number): StyledRow => {
    const row: ReturnType<typeof plain>[] = [plain(INDENT)];
    parts.forEach((part, i) => {
      if (i > 0) row.push(styled("  ·  ", META));
      const original = from + i;
      row.push(
        original < segments.length
          ? styled(part, style(original))
          : styled(
              part,
              part === "⚠ keyless" ? FAILED : connectionStyle(state.connection),
            ),
      );
    });
    return row;
  };

  // Host truncates first (§2.1), then identity, then scope — and the glyph is
  // never in the drop list at all. Dropping whole segments rather than
  // ellipsizing each equally is deliberate: `buzz://rel…` beside a full scope
  // is more useful than three half-legible fragments.
  const room = cols - INDENT.length;
  for (let drop = 0; drop <= segments.length; drop++) {
    const parts = [...segments.slice(drop), ...tail];
    if (displayWidth(parts.join("  ·  ")) <= room) {
      return padRow(join(parts, drop), cols);
    }
  }
  // Below even the glyph's width, truncating it is the only option left — but
  // it is the *last* thing truncated, which is what §2.1 asks for.
  return padRow(
    [
      plain(INDENT),
      styled(
        truncate(tail.join("  ·  "), room),
        connectionStyle(state.connection),
      ),
    ],
    cols,
  );
}

/** Row 2: meter · unread · mentions · DMs. Unread goes last (§2.1). */
export function row2(state: StatuslineState, cols: number): StyledRow {
  const parts = meterParts(state.meter);

  /**
   * A segment of the row: its text, and the spans it draws as.
   *
   * Tagged rather than dispatched on the rendered string. Matching `part ===
   * meter` to decide how to colour a segment is re-deriving structure from its
   * own rendering — the mistake `render/span.ts` exists to avoid — and it is
   * one plural away from a real collision: the drop loop below joins these into
   * a width test, so the *text* has to stay exactly what it was, while the
   * *styling* has to come from what the segment is. Carrying both on one object
   * is what keeps those two facts from drifting.
   */
  interface Segment {
    readonly text: string;
    readonly spans: StyledRow;
  }

  /**
   * A count renders in its attention colour **only when it is non-zero**.
   *
   * `0 mentions` in warning-amber is a false alarm, and a row with three amber
   * zeroes on it teaches the operator to stop reading the row — which costs
   * exactly the signal this band exists to carry. Zero is information, so it
   * stays; it simply says so quietly.
   */
  const count = (text: string, n: number, style: SpanStyle): Segment => ({
    text,
    spans: [styled(text, n > 0 ? style : META)],
  });

  // The bar is drawn as fill + trough so the fill is readable as a quantity.
  // One flat colour makes a 78% bar and a 20% bar look alike at a glance,
  // which is the only thing a meter is for.
  const meter: Segment = {
    text: renderMeter(state.meter),
    spans: [
      styled("[", META),
      styled(parts.filled, METER_FILL),
      styled(parts.empty, METER_EMPTY),
      styled("] ", META),
      styled(parts.percent, META),
    ],
  };
  const unread = count(`${state.unread} unread`, state.unread, UNREAD);
  const mentions = count(
    `${state.mentions} mention${state.mentions === 1 ? "" : "s"}`,
    state.mentions,
    MENTION,
  );
  const dms = count(
    `${state.dms} DM${state.dms === 1 ? "" : "s"}`,
    state.dms,
    UNREAD,
  );

  const spansFor = (which: readonly Segment[]): StyledRow => {
    const row = [plain(INDENT)];
    which.forEach((segment, i) => {
      if (i > 0) row.push(styled("  ·  ", META));
      row.push(...segment.spans);
    });
    return row;
  };

  // Drop order is explicit rather than emergent: mentions and DMs survive, the
  // unread total goes last. A generic right-to-left drop would take DMs first,
  // which inverts the table.
  const candidates: ReadonlyArray<readonly Segment[]> = [
    [meter, unread, mentions, dms],
    [meter, mentions, dms],
    [mentions, dms],
    [mentions],
  ];
  const room = cols - INDENT.length;
  for (const which of candidates) {
    const line = which.map((s) => s.text).join("  ·  ");
    if (displayWidth(line) <= room) return padRow(spansFor(which), cols);
  }
  return padRow(
    [plain(INDENT), styled(truncate(mentions.text, room), MENTION)],
    cols,
  );
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
export function row3(state: StatuslineState, cols: number): StyledRow {
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

  /**
   * The live counts carry colour; the key hint recedes.
   *
   * This is [G13]'s "the door is labeled only when there is something behind
   * it" made visible. When agents are working, that fact is the row's content
   * and the arrows are the instructions for reaching it — so the count is the
   * only thing on the row that is not muted. When nothing is live, the whole
   * row is instruction and all of it recedes, which is what stops a permanently
   * visible hint from reading as a permanent alert.
   */
  // Each part carries its own style rather than being looked up in `live` by
  // string membership: a hint that happened to equal a count's text would take
  // the wrong colour, and more to the point, whether a part is a live count is
  // known where it is built, not where it is drawn.
  type Part = readonly [text: string, style: SpanStyle];
  const liveParts: Part[] = live.map((text) => [text, LIVE]);
  const hintPart: Part = [hint, HINT];

  const spansFor = (parts: readonly Part[]): StyledRow => {
    const row = [plain(INDENT), styled("⏵ ", META)];
    parts.forEach(([text, style], i) => {
      if (i > 0) row.push(styled(" · ", META));
      row.push(styled(text, style));
    });
    return row;
  };

  const room = cols - INDENT.length;
  const full = `⏵ ${[...live, hint].join(" · ")}`;
  if (displayWidth(full) <= room)
    return padRow(spansFor([...liveParts, hintPart]), cols);

  if (live.length > 0) {
    const countsOnly = `⏵ ${live.join(" · ")}`;
    if (displayWidth(countsOnly) <= room)
      return padRow(spansFor(liveParts), cols);
    return padRow(
      [
        plain(INDENT),
        styled("⏵ ", META),
        styled(truncate(live.join(" · "), Math.max(0, room - 2)), LIVE),
      ],
      cols,
    );
  }
  const hintOnly = `⏵ ${hint}`;
  if (displayWidth(hintOnly) <= room) return padRow(spansFor([hintPart]), cols);
  return padRow(
    [
      plain(INDENT),
      styled("⏵ ", META),
      styled(truncate(hint, Math.max(0, room - 2)), HINT),
    ],
    cols,
  );
}

/** The full three-row band. Always exactly three rows, at every width [G11]. */
export function renderStatusline(
  state: StatuslineState,
  cols: number,
): StyledRow[] {
  return [row1(state, cols), row2(state, cols), row3(state, cols)];
}

/** The band's height. Fixed — it is what the body's flex height subtracts. */
export const STATUSLINE_ROWS = 3;
