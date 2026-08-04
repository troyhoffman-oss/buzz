/**
 * Responsive tiers — DESIGN.md §3.9.
 *
 * Explicit tiers with a distinct tiny fallback, not a flex solver squeezing
 * panes into illegibility. **The XS and SM tiers are the phone-via-Moshi
 * experience and are Wave-1 requirements, not polish** (§1.2: the phone is one
 * of the two primary reading surfaces).
 *
 * Per-breakpoint value resolution is **table-driven, not scattered
 * `if (width < 80)` checks** (§3.9).
 */

/** Tier names, widest to narrowest. */
export type Tier = "xl" | "lg" | "md" | "mdn" | "sm" | "xs";

/** Which of the four §3.0 regions a tier draws. */
export interface TierLayout {
  /** Community rail. */
  rail: boolean;
  /** Channel/inbox list. Width 18 when collapsed (MD-narrow). */
  list: "full" | "collapsed" | "dialog" | "none";
  /** Aux pane: inline, an overlay, or unavailable. */
  aux: "pane" | "overlay" | "none";
}

/**
 * Minimum supported terminal size (§3.9).
 *
 * An earlier draft declared 80×24 while also making SM and XS Wave-1
 * requirements — i.e. it declared the phone tier both required and unsupported.
 * §1.2 settles it: XS is a product tier with an SLA, not a degradation. Below
 * this the app renders one legible line and **keeps running** — it does not
 * exit and does not panic.
 */
export const MIN_COLS = 40;
/** @see MIN_COLS */
export const MIN_ROWS = 16;

/** Width of the MD-narrow collapsed list: glyph + unread count, no names. */
export const COLLAPSED_LIST_COLS = 18;

/**
 * Tier breakpoints, widest first. Resolution walks this in order and takes the
 * first whose floor the width clears.
 *
 * **MD-narrow exists because 72 columns is the everyday case** (§3.9): half of
 * a 144-column terminal is the single most common vertical split, and the
 * draft's MD ≥90 → SM ≥60 jump dropped it to "main only" — losing the channel
 * list, the agent rail, unread visibility, and presence glyphs at precisely the
 * width these operators work at. Losing channel *names* is much cheaper than
 * losing channel *context*.
 */
export const TIERS: ReadonlyArray<{
  tier: Tier;
  minCols: number;
  layout: TierLayout;
}> = [
  {
    tier: "xl",
    minCols: 160,
    layout: { rail: true, list: "full", aux: "pane" },
  },
  {
    tier: "lg",
    minCols: 120,
    layout: { rail: false, list: "full", aux: "pane" },
  },
  {
    tier: "md",
    minCols: 90,
    layout: { rail: false, list: "full", aux: "overlay" },
  },
  {
    tier: "mdn",
    minCols: 72,
    layout: { rail: false, list: "collapsed", aux: "overlay" },
  },
  {
    tier: "sm",
    minCols: 60,
    layout: { rail: false, list: "dialog", aux: "overlay" },
  },
  {
    tier: "xs",
    minCols: 0,
    layout: { rail: false, list: "none", aux: "none" },
  },
];

/** Resolve the tier for a terminal width. */
export function tierFor(cols: number): Tier {
  for (const entry of TIERS) {
    if (cols >= entry.minCols) return entry.tier;
  }
  return "xs";
}

/** Resolve the region layout for a terminal width. */
export function layoutFor(cols: number): TierLayout {
  const tier = tierFor(cols);
  const entry = TIERS.find((t) => t.tier === tier);
  if (!entry) throw new Error(`unreachable: no layout for tier ${tier}`);
  return entry.layout;
}

/** Whether the terminal is below the supported floor (§3.9). */
export function isBelowFloor(cols: number, rows: number): boolean {
  return cols < MIN_COLS || rows < MIN_ROWS;
}

/**
 * Status-bar segments in **drop priority order**, left to right (§3.9).
 *
 * Segments drop right-to-left, and **the first two never drop, at any tier**.
 * Without a stated order the segment that truncates on the phone could be the
 * connection indicator — failing §1.3 property 3 exactly where it matters most
 * — or the pending-leader indicator, which §3.7 calls the number-one way a
 * keybinding grammar feels broken.
 */
export const STATUS_SEGMENTS = [
  "connection",
  "pending-leader",
  "mentions",
  "unread",
  "agent-usage-strip",
  "context",
  "help",
] as const;

/** A status-bar segment name. */
export type StatusSegment = (typeof STATUS_SEGMENTS)[number];

/** Segments that survive every tier. */
export const UNDROPPABLE_SEGMENTS: readonly StatusSegment[] = [
  "connection",
  "pending-leader",
];

/**
 * Choose which status segments fit, dropping right-to-left.
 *
 * `widths` gives each segment's rendered width. The first two are always
 * returned even when they do not fit — a truncated connection indicator is a
 * worse failure than an overflowing status bar.
 */
export function visibleSegments(
  available: number,
  widths: Readonly<Record<StatusSegment, number>>,
): StatusSegment[] {
  const kept: StatusSegment[] = [...UNDROPPABLE_SEGMENTS];
  let used = kept.reduce((sum, s) => sum + widths[s], 0);
  for (const segment of STATUS_SEGMENTS) {
    if (kept.includes(segment)) continue;
    const next = used + widths[segment];
    if (next > available) break;
    kept.push(segment);
    used = next;
  }
  return kept;
}
