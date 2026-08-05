/**
 * L3 THREAD — NAVIGATION.md §1, §4.2, and DESIGN.md §3.2 as superseded by §7.
 *
 * §7 collapses the desktop's three thread modes:
 *
 * > **§3.2 Thread view** — superseded. `docked`/`focused`/`detached` collapse
 * > to one thing: L3, full screen. "Detached" is served by the back stack
 * > remembering the path in, not by a persistent pane.
 *
 * That is the whole simplification: a thread you parked and came back to is a
 * back-stack entry, not a pane that has to survive channel navigation. What
 * survives from §3.2 is the *tree shape* — `└` guides capped at three visual
 * indent levels, and an explicit backfill affordance for orphans.
 */

import type { Message } from "../client/types";
import {
  AGENT,
  AUTHOR,
  CHROME,
  FOCUS,
  HINT,
  META,
  THREAD,
} from "../render/palette";
import {
  type Span,
  type SpanStyle,
  type StyledRow,
  padRow,
  plain,
  rowText,
  splitAt,
  styled,
} from "../render/span";
import { displayWidth, truncateKeepingSuffix, wrapText } from "../render/width";
import { threadSuffix } from "../render/timeline";

/**
 * Maximum visual indent, from `threadTreeLayout.ts` (§3.2).
 *
 * > capped at 3 visual indent levels (deeper replies stay at level 3 with a
 * > `·3` depth marker) so a deep thread does not become a 1-column column.
 *
 * At 60 columns this matters more, not less: three levels is already 6 columns
 * of the 60, and an uncapped tree would leave a reply five deep with nothing to
 * wrap into.
 */
export const MAX_INDENT = 3;

/** Columns per indent level. */
const INDENT_COLS = 2;

/** A reply with its resolved depth in the tree. */
export interface ThreadNode {
  readonly message: Message;
  /** True depth in the reply graph, uncapped. */
  readonly depth: number;
  /** True when the parent is not in the loaded set (§3.2's orphan case). */
  readonly orphan: boolean;
}

/**
 * Build the reply tree for a root.
 *
 * Depth-first in timestamp order, so a thread reads top-to-bottom the way it
 * happened. Orphans — replies whose parent is not loaded — are surfaced
 * **explicitly** rather than silently hidden or silently promoted to depth 1:
 * §3.2 makes `useLoadMissingAncestors`'s behaviour visible, and a reply that
 * quietly reparents itself misattributes the conversation.
 */
export function buildThread(
  root: Message,
  all: readonly Message[],
): ThreadNode[] {
  const loaded = new Set(all.map((m) => m.id));
  const byParent = new Map<string, Message[]>();
  for (const message of all) {
    if (!message.replyTo) continue;
    const siblings = byParent.get(message.replyTo) ?? [];
    siblings.push(message);
    byParent.set(message.replyTo, siblings);
  }
  for (const siblings of byParent.values())
    siblings.sort((a, b) => a.ts - b.ts);

  const nodes: ThreadNode[] = [{ message: root, depth: 0, orphan: false }];

  const walk = (parentId: string, depth: number): void => {
    for (const child of byParent.get(parentId) ?? []) {
      nodes.push({ message: child, depth, orphan: false });
      walk(child.id, depth + 1);
    }
  };
  walk(root.id, 1);

  // Orphans: in this thread's channel, replying to something not loaded.
  const placed = new Set(nodes.map((n) => n.message.id));
  for (const message of all) {
    if (placed.has(message.id)) continue;
    if (!message.replyTo || loaded.has(message.replyTo)) continue;
    nodes.push({ message, depth: 1, orphan: true });
  }

  return nodes;
}

/**
 * Render the thread body (§3.2's shape, §7's full-screen placement).
 *
 * The `└` guides are the thing colour helps most here. They are pure structure
 * — three levels of them, two columns each — and drawn at the same weight as
 * the replies they organise they read as punctuation inside the text. Receding
 * them to `borderSubtle` is what lets the tree shape register peripherally
 * while the words stay the only thing you actually read, which is the same
 * trade `render/timeline.ts` makes for the day divider.
 */
export function renderThread(
  nodes: readonly ThreadNode[],
  cols: number,
  selectedMessageId?: string,
): StyledRow[] {
  const rows: StyledRow[] = [];
  for (const node of nodes) {
    const visual = Math.min(node.depth, MAX_INDENT);
    const indent = " ".repeat(visual * INDENT_COLS);
    const guide = node.depth > 0 ? "└ " : "";
    const overflow = node.depth > MAX_INDENT ? ` ·${node.depth}` : "";
    const selected = selectedMessageId === node.message.id;
    const marker = selected ? "❯" : " ";
    const time = formatClock(node.message.ts);

    const head = truncateKeepingSuffix(
      `${node.message.author.name}${overflow}`,
      time,
      Math.max(1, cols - 2 - indent.length - guide.length),
    );
    // The author line is `name … time`, laid out by `truncateKeepingSuffix`,
    // so the stamp is the tail and the name is everything before it. Cutting
    // the finished string keeps the layout function the only place that
    // arithmetic lives (see `render/span.ts`).
    const nameStyle = node.message.author.isAgent ? AGENT : AUTHOR;
    const [namePart, timePart] = head.endsWith(time)
      ? splitAt([plain(head)], displayWidth(head) - displayWidth(time))
      : [[plain(head)], []];
    rows.push(
      padRow(
        [
          styled(marker, selected ? FOCUS : {}),
          plain(` ${indent}`),
          styled(guide, CHROME),
          styled(rowText(namePart), nameStyle),
          ...(timePart.length > 0 ? [styled(rowText(timePart), META)] : []),
        ],
        cols,
      ),
    );

    const bodyIndent = " ".repeat(visual * INDENT_COLS + guide.length);
    const suffix = threadSuffix(node.message);
    const wrapped = wrapText(
      node.message.content,
      Math.max(1, cols - 2 - bodyIndent.length),
      0,
    );
    wrapped.forEach((line, i) => {
      const isLast = i === wrapped.length - 1;
      const text =
        isLast && suffix
          ? truncateKeepingSuffix(
              line,
              suffix,
              Math.max(1, cols - 2 - bodyIndent.length),
            )
          : line;
      // Reply text is content and stays unstyled; only a thread's own counts
      // are attributed, for the reason §3's list-row rule gives — they are
      // what you pick a conversation by, not part of what it says.
      const carries = isLast && suffix.length > 0 && text.endsWith(suffix);
      const [body, tail] = carries
        ? splitAt([plain(text)], displayWidth(text) - displayWidth(suffix))
        : [[plain(text)], []];
      rows.push(
        padRow(
          [
            plain(`  ${bodyIndent}`),
            plain(rowText(body)),
            ...(carries ? [styled(rowText(tail), THREAD)] : []),
          ],
          cols,
        ),
      );
    });

    if (node.orphan) {
      // §3.2: "Orphan replies whose parent is not loaded render an explicit
      // backfill affordance rather than silently hiding."
      //
      // The box rules are chrome and the two lines inside it are a statement
      // plus its remedy — so the notice is muted and the key is a hint, and
      // **nothing here is red**. An orphan is not a failure: the ancestors
      // exist and `^X b` fetches them, which is precisely why §3.2 pairs the
      // notice with a key rather than with an apology. Drawing a recoverable
      // gap in the same colour as `auth failed` is the "Christmas tree" §3.10
      // names — and worse, it spends the one token that has to still mean
      // something when the socket really is dead.
      const box = (glyph: string, label: string, style: SpanStyle): Span[] => [
        plain(`  ${bodyIndent}`),
        styled(glyph, CHROME),
        styled(label, style),
      ];
      rows.push(padRow(box("┌ ", "orphan — parent not loaded", META), cols));
      rows.push(padRow(box("│  ", "^X b  backfill ancestors", HINT), cols));
      rows.push(padRow([plain(`  ${bodyIndent}`), styled("└", CHROME)], cols));
    }
  }
  return rows;
}

/** `HH:MM` in UTC. See `layers/home.ts` for why UTC and not local. */
function formatClock(ts: number): string {
  const date = new Date(ts);
  return `${String(date.getUTCHours()).padStart(2, "0")}:${String(date.getUTCMinutes()).padStart(2, "0")}`;
}
