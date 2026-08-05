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
  CHROME,
  DEGRADED,
  FAILED,
  FOCUS,
  HINT,
  LIVE,
  MENTION,
  META,
  SECTION,
  SELECTED,
  THREAD,
} from "./palette";
import {
  type Span,
  type SpanStyle,
  type StyledRow,
  fillRow,
  padRow,
  plain,
  rowText,
  splitAt,
  styled,
} from "./span";
import {
  alignRight,
  displayWidth,
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

/**
 * Render the drawer list (§2.3), selection marked with `❯`.
 *
 * **`selected` indexes `rows`, not the fitted subset.** Degradation can drop
 * rows ({@link fitDrawerRows}), so an index into the *rendered* list would mean
 * something different from the index the reducer holds — the `❯` would sit on
 * one row while `→` committed to another. That desync is invisible until the
 * drawer degrades, which is exactly when a user is least able to tell a
 * mis-navigation from a crowded screen. Matching by identity rather than by
 * position is what keeps the two in agreement at every height.
 */
export function renderDrawerList(
  rows: readonly DrawerRow[],
  selected: number,
  cols: number,
  availableRows: number,
): StyledRow[] {
  const hints = wrapHints([...DRAWER_HINTS], cols);
  // Reserve: the `Live` header, the summary, a blank, the trailing blank, and
  // the hint rows. What is left is what the list itself may use.
  const listRows = Math.max(1, availableRows - 4 - hints.length);
  const fitted = fitDrawerRows(rows, listRows);
  const selectedId = rows[selected]?.id;

  const out: StyledRow[] = [
    // The drawer replaces the statusline, so its own title is the only thing
    // naming the surface you are now looking at — it stays at base weight
    // while the summary beneath it, which is a count rather than a label,
    // recedes.
    padRow([plain("  Live")], cols),
    padRow([plain("  "), styled(liveSummary(rows), META)], cols),
    padRow([], cols),
  ];

  let lastSection: DrawerSection | null = null;
  fitted.rows.forEach((row) => {
    if (!fitted.compactHeaders && row.section !== lastSection) {
      out.push(padRow([plain("  "), styled(row.section, SECTION)], cols));
      lastSection = row.section;
    }
    const isSelected = row.id === selectedId;
    const marker = isSelected ? "❯ " : "  ";
    const label = row.detail ? `${row.label}   ${row.detail}` : row.label;
    const text = truncateKeepingSuffix(label, row.status, cols - 2);
    // The status suffix is the [G12] ladder made visible: it is the reason the
    // row is in the drawer at all, and §2.3 orders the whole list by it. So it
    // is the one part of a drawer row that carries colour, and the label —
    // an agent's name, a thread's title — stays neutral text.
    const carries = row.status.length > 0 && text.endsWith(row.status);
    const [head, tail] = carries
      ? splitAt([plain(text)], displayWidth(text) - displayWidth(row.status))
      : [[plain(text)], []];
    const body: Span[] = [
      isSelected ? styled(marker, FOCUS) : plain(marker),
      // A SYSTEM row's "label" *is* its message — a degradation notice, not a
      // name — so it takes the status colour it has no suffix to put it on.
      row.section === "SYSTEM"
        ? styled(rowText(head), systemStyle(row.label))
        : plain(rowText(head)),
      ...(carries ? [styled(rowText(tail), statusStyle(row))] : []),
    ];
    out.push(isSelected ? fillRow(body, cols, SELECTED) : padRow(body, cols));
  });

  if (fitted.collapsed > 0) {
    out.push(
      padRow([plain("  "), styled(`… ${fitted.collapsed} more`, META)], cols),
    );
  }
  out.push(padRow([], cols));
  for (const hint of hints)
    out.push(padRow([plain("  "), styled(hint, HINT)], cols));
  return out;
}

/**
 * The colour a drawer row's status suffix takes.
 *
 * Keyed on the section rather than on the string, so a thread whose title
 * happened to read `working` cannot borrow an agent's colour — the section is
 * what the row *is*, and it is known where the row is built.
 *
 * `needs input` is the loudest thing the drawer can say, and it is the only
 * status that gets the mention colour: it is the top of §2.3's attention
 * ladder for the same reason a mention is — something is blocked waiting for
 * *you*, and it will stay blocked until you act. `working` is green because it
 * needs nothing; `failed` is red because it is over.
 */
function statusStyle(row: DrawerRow): SpanStyle {
  if (row.section === "THREADS") return THREAD;
  if (row.section === "HUDDLE") return LIVE;
  if (row.section === "SYSTEM") return systemStyle(row.label);
  switch (row.status) {
    case "needs input":
      return MENTION;
    case "working":
      return LIVE;
    case "failed":
      return FAILED;
    default:
      return META;
  }
}

/**
 * The colour a SYSTEM row takes.
 *
 * Two degradations reach this section (`app/screen.ts`'s `drawerRows`), and
 * they are not the same kind of bad. A reconnecting relay resolves itself by
 * waiting; a daemon with no identity **never** does — §2.5 puts it here
 * precisely because a keyless daemon that looks healthy is the failure the
 * state exists to prevent, and amber would say "in progress" about something
 * that requires the operator to go and fix it.
 */
function systemStyle(label: string): SpanStyle {
  return label.includes("no identity") ? FAILED : DEGRADED;
}

/** The expanded detail view's data (§2.4). */
export interface DrawerExpansion {
  readonly title: string;
  /** Aligned label/value block — zone 1 of §2.4's two zones. */
  readonly fields: ReadonlyArray<readonly [string, string]>;
  /**
   * The live tail — zone 2, a fixed-height box that scrolls in place.
   *
   * Styled rows, because they come from `renderTranscript` and §6's whole claim
   * is that the drawer's expansion and L4 render the *same* rows. Taking plain
   * strings here would have flattened the transcript's class glyphs on one of
   * the two paths — the version of "same rows" that is true of the text and
   * false of the screen.
   */
  readonly tail: readonly StyledRow[];
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
): StyledRow[] {
  const height = expansionHeight(terminalRows);
  const hints = wrapHints([...EXPANSION_HINTS], cols);

  const out: StyledRow[] = [
    padRow([plain(`  ${expansion.title}`)], cols),
    padRow([], cols),
  ];

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
      // Label muted, value at base weight. The block is read by scanning down
      // the values — the turn id, the token counts — and a label column at the
      // same weight makes the eye stop at every row twice.
      out.push(padRow([plain("  "), styled(label, META), plain(line)], cols));
    });
  }

  out.push(padRow([], cols));
  out.push(padRow([plain("  Transcript")], cols));

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
    // §1.3 property 3 in miniature: an empty tail is a *statement*, muted so it
    // reads as one rather than as a box that failed to draw.
    out.push(padRow([plain("  "), styled("no frames yet", META)], cols));
    for (const hint of hints)
      out.push(padRow([plain("  "), styled(hint, HINT)], cols));
    return out;
  }

  const end = Math.max(0, expansion.tail.length - expansion.tailOffset);
  const start = Math.max(0, end - tailRows);
  const window = expansion.tail.slice(start, end);

  // The box is chrome and recedes; the transcript inside it is the content and
  // does not. Drawn at one weight the frame competes with the frames it holds,
  // which on the narrowest terminal is most of what you can see.
  out.push(
    padRow([plain("  "), styled(`╭${"─".repeat(inner)}╮`, CHROME)], cols),
  );
  for (let i = 0; i < tailRows; i++) {
    // The tail row keeps its own spans inside the box — this is §6's "one
    // renderable" made literal: the class glyph an operator learned in L4 is
    // the same colour here. Padding the row to the box's inner width *before*
    // the right edge is what keeps the frame vertical when a transcript line
    // is shorter than the box.
    const line: StyledRow = window[i] ?? [];
    out.push(
      padRow(
        [
          plain("  "),
          styled("│", CHROME),
          ...padRow([plain(" "), ...line], inner),
          styled("│", CHROME),
        ],
        cols,
      ),
    );
  }
  out.push(
    padRow([plain("  "), styled(`╰${"─".repeat(inner)}╯`, CHROME)], cols),
  );
  // The `Showing N of M` footer is the box telling you it shrank — §2.4's
  // fixed-height window made honest. It is metadata about the view, not
  // content, so it recedes with everything else of that class.
  out.push(
    padRow(
      [
        plain("  "),
        styled(
          alignRight(
            `Showing ${window.length} of ${expansion.tailTotal} lines`,
            "",
            cols - 2,
          ),
          META,
        ),
      ],
      cols,
    ),
  );
  for (const hint of hints)
    out.push(padRow([plain("  "), styled(hint, HINT)], cols));
  return out;
}
