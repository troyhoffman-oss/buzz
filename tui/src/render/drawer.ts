/**
 * The drawer — NAVIGATION.md §2.3 and §2.4.
 *
 * > `↓` **replaces the composer and statusline** and grows upward, compressing
 * > the body. It does not overlay, and it does not open downward: nothing in
 * > this design opens downward, nothing floats, nothing is centered [G1].
 *
 * Row order is the [G12] attention ladder, **verbatim**:
 * `needs-input > working > failed > header chrome > completed`.
 *
 * And the piece that most often gets built wrong:
 *
 * > The tail box is a **fixed-height window that scrolls its content in
 * > place** — the box does not grow. Height is capped at
 * > `min(18, floor(H * 0.5))` and the tail window shrinks to fit, fixing the
 * > measured Claude Code failure where a 20-row detail view overflowed a 16-row
 * > terminal. **The chat is never starved.**
 */

import type { Agent, AgentState, Huddle, LiveThread } from "../client/types";
import {
  alignRight,
  pad,
  truncateKeepingSuffix,
  wrapHints,
  wrapText,
} from "./width";

/** A section of the drawer list (§2.3's typed sections). */
export type DrawerSection = "AGENTS" | "THREADS" | "HUDDLE" | "SYSTEM";

/** One selectable row in the drawer. */
export interface DrawerRow {
  readonly id: string;
  readonly section: DrawerSection;
  /** Left-hand label — the name. Ellipsizes under narrowing. */
  readonly label: string;
  /** Middle detail. Dropped before the status suffix. */
  readonly detail: string;
  /** Right-hand status. Survives truncation (§3's list-row rule). */
  readonly status: string;
  /** Ladder rank; see {@link ATTENTION_LADDER}. */
  readonly rank: number;
  /** What `→` commits to (§2.4). */
  readonly target:
    | { kind: "activity"; agentPubkey: string; channelId?: string }
    | { kind: "thread"; rootEventId: string; channelId: string }
    | { kind: "none" };
}

/**
 * The [G12] attention ladder — needs-input > working > failed > completed.
 *
 * "header chrome" sits between `failed` and `completed` in the spec's list; it
 * is not a row rank but the section headers themselves, which is why they
 * compact before any live row is dropped ({@link fitDrawerRows}).
 */
export const ATTENTION_LADDER: Readonly<Record<AgentState, number>> = {
  needsInput: 0,
  working: 1,
  failed: 2,
  idle: 3,
  completed: 4,
};

/** Build the drawer's rows from live state, in ladder order (§2.3). */
export function buildDrawerRows(
  agents: readonly Agent[],
  threads: readonly LiveThread[],
  huddles: readonly Huddle[],
  system: readonly string[],
): DrawerRow[] {
  const agentRows: DrawerRow[] = agents.map((agent) => ({
    id: `agent:${agent.pubkey}`,
    section: "AGENTS",
    label: agent.name,
    detail: [agent.channelName ?? "", agent.detail ?? ""]
      .filter(Boolean)
      .join("   "),
    status: statusLabel(agent.state),
    rank: ATTENTION_LADDER[agent.state],
    target: agent.channelId
      ? {
          kind: "activity",
          agentPubkey: agent.pubkey,
          channelId: agent.channelId,
        }
      : { kind: "activity", agentPubkey: agent.pubkey },
  }));
  // Sort is stable in every JS engine since ES2019, so agents at the same rank
  // keep the daemon's order — which is its own attention ordering. Re-sorting
  // by name here would discard that.
  agentRows.sort((a, b) => a.rank - b.rank);

  const threadRows: DrawerRow[] = threads.map((thread) => ({
    id: `thread:${thread.rootEventId}`,
    section: "THREADS",
    label: thread.title,
    detail: thread.channelName,
    status: `${thread.replyCount} replies · ${thread.newCount} new`,
    rank: thread.newCount > 0 ? 1 : 4,
    target: {
      kind: "thread",
      rootEventId: thread.rootEventId,
      channelId: thread.channelId,
    },
  }));

  const huddleRows: DrawerRow[] = huddles.map((huddle) => ({
    id: `huddle:${huddle.id}`,
    section: "HUDDLE",
    label: huddle.name,
    detail: `${huddle.participants} participants`,
    status: "live",
    rank: 1,
    target: { kind: "none" },
  }));

  const systemRows: DrawerRow[] = system.map((line, i) => ({
    id: `system:${i}`,
    section: "SYSTEM",
    label: line,
    detail: "",
    status: "",
    rank: 2,
    target: { kind: "none" },
  }));

  return [...agentRows, ...threadRows, ...huddleRows, ...systemRows];
}

function statusLabel(state: AgentState): string {
  switch (state) {
    case "needsInput":
      return "needs input";
    case "working":
      return "working";
    case "failed":
      return "failed";
    case "completed":
      return "completed";
    case "idle":
      return "idle";
  }
}

/** The drawer's summary line: `3 agents working · 2 live threads · 1 huddle`. */
export function liveSummary(rows: readonly DrawerRow[]): string {
  const working = rows.filter(
    (r) =>
      r.section === "AGENTS" &&
      (r.status === "working" || r.status === "needs input"),
  ).length;
  const threads = rows.filter((r) => r.section === "THREADS").length;
  const huddles = rows.filter((r) => r.section === "HUDDLE").length;
  const parts: string[] = [];
  if (working > 0)
    parts.push(`${working} agent${working === 1 ? "" : "s"} working`);
  if (threads > 0)
    parts.push(`${threads} live thread${threads === 1 ? "" : "s"}`);
  if (huddles > 0) parts.push(`${huddles} huddle${huddles === 1 ? "" : "s"}`);
  return parts.length > 0 ? parts.join(" · ") : "nothing running";
}

/** The drawer's key hints. Wrap, never truncate [G14]. */
export const DRAWER_HINTS = [
  "↑/↓ select",
  "⏎ expand",
  "→ jump in",
  "esc close",
] as const;

/**
 * Choose which rows fit — §2.3's degradation order.
 *
 * > When the drawer cannot fit, completed and idle rows collapse into a count
 * > (`… 4 more`), section headers compact to a single summary line before any
 * > live row is dropped, and the key-hint footer **wraps rather than
 * > truncates**.
 *
 * The order matters and is easy to invert: a naive "drop from the bottom" would
 * take the SYSTEM row (`relay reconnecting`) before a completed agent, which is
 * exactly backwards — the ladder puts chrome above completed work.
 */
export interface FittedDrawer {
  readonly rows: readonly DrawerRow[];
  /** True when headers are suppressed to buy rows. */
  readonly compactHeaders: boolean;
  /** How many rows collapsed into the `… N more` line. */
  readonly collapsed: number;
}

/** Rank at or above which a row is "completed or idle" and collapses first. */
const COLLAPSIBLE_RANK = 3;

export function fitDrawerRows(
  rows: readonly DrawerRow[],
  availableRows: number,
): FittedDrawer {
  const sectionCount = new Set(rows.map((r) => r.section)).size;

  if (availableRows >= rows.length + sectionCount) {
    return { rows, compactHeaders: false, collapsed: 0 };
  }

  // Step 1: collapse completed/idle rows into a count, keeping every live row.
  const live = rows.filter((r) => r.rank < COLLAPSIBLE_RANK);
  const collapsed = rows.length - live.length;
  const liveSections = new Set(live.map((r) => r.section)).size;
  if (collapsed > 0 && availableRows >= live.length + liveSections + 1) {
    return { rows: live, compactHeaders: false, collapsed };
  }

  // Step 2: compact the section headers. Live rows are still all present.
  if (availableRows >= live.length + 1) {
    return { rows: live, compactHeaders: true, collapsed };
  }

  // Step 3: only now drop live rows, ladder-worst first — and keep at least
  // one, because a drawer showing zero of three working agents is worse than
  // a drawer showing one and saying so.
  const keep = Math.max(1, availableRows - 1);
  const kept = [...live].sort((a, b) => a.rank - b.rank).slice(0, keep);
  return {
    rows: kept,
    compactHeaders: true,
    collapsed: collapsed + (live.length - kept.length),
  };
}

/**
 * Rows the drawer list wants, given its content — §2.3's frame.
 *
 * `Live` + summary + blank + one row per entry + one per section header +
 * blank + hints. The band is **content-derived and then capped** by the caller
 * ({@link expansionHeight}), rather than fixed at some number: §2.3 puts
 * dropping rows last in its degradation order, so a band that was fixed small
 * would compact headers and collapse rows on a tall terminal that had plenty of
 * space for them — degrading for no reason.
 */
export function drawerListHeight(
  rows: readonly DrawerRow[],
  cols: number,
): number {
  const sections = new Set(rows.map((r) => r.section)).size;
  return (
    3 + rows.length + sections + 1 + wrapHints([...DRAWER_HINTS], cols).length
  );
}

/** Render the drawer list (§2.3), selection marked with `❯`. */
export function renderDrawerList(
  rows: readonly DrawerRow[],
  selected: number,
  cols: number,
  availableRows: number,
): string[] {
  const hints = wrapHints([...DRAWER_HINTS], cols);
  // Reserve: the `Live` header, the summary, a blank, the trailing blank, and
  // the hint rows. What is left is what the list itself may use.
  const listRows = Math.max(1, availableRows - 4 - hints.length);
  const fitted = fitDrawerRows(rows, listRows);

  const out: string[] = [
    pad("  Live", cols),
    pad(`  ${liveSummary(rows)}`, cols),
    pad("", cols),
  ];

  let lastSection: DrawerSection | null = null;
  fitted.rows.forEach((row, index) => {
    if (!fitted.compactHeaders && row.section !== lastSection) {
      out.push(pad(`  ${row.section}`, cols));
      lastSection = row.section;
    }
    const marker = index === selected ? "❯ " : "  ";
    const label = row.detail ? `${row.label}   ${row.detail}` : row.label;
    out.push(
      pad(
        `${marker}${truncateKeepingSuffix(label, row.status, cols - 2)}`,
        cols,
      ),
    );
  });

  if (fitted.collapsed > 0) out.push(pad(`  … ${fitted.collapsed} more`, cols));
  out.push(pad("", cols));
  for (const hint of hints) out.push(pad(`  ${hint}`, cols));
  return out;
}

/** The expanded detail view's data (§2.4). */
export interface DrawerExpansion {
  readonly title: string;
  /** Aligned label/value block — zone 1 of §2.4's two zones. */
  readonly fields: ReadonlyArray<readonly [string, string]>;
  /** The live tail — zone 2, a fixed-height box that scrolls in place. */
  readonly tail: readonly string[];
  /** How far the tail is scrolled from its end. 0 == pinned to the newest. */
  readonly tailOffset: number;
  /** Total lines behind the tail window, for the `Showing 9 of 214` footer. */
  readonly tailTotal: number;
}

/**
 * The expansion's height cap — §2.4, verbatim.
 *
 * > Height is capped at `min(18, floor(H * 0.5))` and the tail window shrinks
 * > to fit.
 *
 * The `H * 0.5` half is the one that fixes the measured Claude Code failure: a
 * fixed 18 on a 16-row terminal overflows, and the overflow evicts the chat the
 * peek exists to avoid leaving.
 */
export function expansionHeight(terminalRows: number): number {
  return Math.max(1, Math.min(18, Math.floor(terminalRows * 0.5)));
}

/** The expansion's hints. Wrap, never truncate [G14]. */
export const EXPANSION_HINTS = [
  "→ open full activity",
  "← back",
  "esc close",
] as const;

/**
 * Render the in-place expansion (§2.4).
 *
 * Two zones: an aligned label/value block, then one boxed live tail. The box
 * does not grow — when the height cap tightens, the *window* shrinks and the
 * `Showing N of M` footer tells you it did. A box that grew instead would push
 * the chat out, and §2.4's whole claim is `[G16]`: "The chat is compressed,
 * never evicted."
 */
export function renderDrawerExpansion(
  expansion: DrawerExpansion,
  cols: number,
  terminalRows: number,
): string[] {
  const height = expansionHeight(terminalRows);
  const hints = wrapHints([...EXPANSION_HINTS], cols);

  const out: string[] = [pad(`  ${expansion.title}`, cols), pad("", cols)];

  const labelWidth =
    Math.max(...expansion.fields.map(([k]) => k.length), 0) + 2;
  for (const [key, value] of expansion.fields) {
    // §7: "detail fields wrap with hanging indent". A field that truncated
    // would hide the turn id or the usage numbers, which are the reason the
    // block exists.
    const wrapped = wrapText(value, Math.max(1, cols - labelWidth - 2), 0);
    wrapped.forEach((line, i) => {
      const label =
        i === 0 ? `${key}:`.padEnd(labelWidth) : " ".repeat(labelWidth);
      out.push(pad(`  ${label}${line}`, cols));
    });
  }

  out.push(pad("", cols));
  out.push(pad("  Transcript", cols));

  // Everything above plus the box chrome, the footer, and the hints is fixed
  // overhead; the tail window is whatever is left, floored at one row so the
  // box never renders as two rules with nothing between them.
  const overhead = out.length + 2 + 1 + hints.length;
  const tailRows = Math.max(1, height - overhead);
  const inner = Math.max(1, cols - 4);

  // An empty tail renders as one labelled row, not as a box of blank lines.
  // DESIGN §1.3 property 3: "loss is always visible … never silence." A framed
  // void does not distinguish "this agent has no transcript" from "the frames
  // have not arrived yet", and it spends five rows of the chat's height saying
  // nothing.
  if (expansion.tail.length === 0) {
    out.push(pad("  no frames yet", cols));
    for (const hint of hints) out.push(pad(`  ${hint}`, cols));
    return out;
  }

  const end = Math.max(0, expansion.tail.length - expansion.tailOffset);
  const start = Math.max(0, end - tailRows);
  const window = expansion.tail.slice(start, end);

  out.push(pad(`  ╭${"─".repeat(inner)}╮`, cols));
  for (let i = 0; i < tailRows; i++) {
    const line = window[i] ?? "";
    out.push(pad(`  │${pad(` ${line}`, inner)}│`, cols));
  }
  out.push(pad(`  ╰${"─".repeat(inner)}╯`, cols));
  out.push(
    pad(
      `  ${alignRight(`Showing ${window.length} of ${expansion.tailTotal} lines`, "", cols - 2)}`,
      cols,
    ),
  );
  for (const hint of hints) out.push(pad(`  ${hint}`, cols));
  return out;
}
