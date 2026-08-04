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
import { pad, truncateKeepingSuffix, wrapText } from "../render/width";
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

/** Render the thread body (§3.2's shape, §7's full-screen placement). */
export function renderThread(
  nodes: readonly ThreadNode[],
  cols: number,
  selectedMessageId?: string,
): string[] {
  const rows: string[] = [];
  for (const node of nodes) {
    const visual = Math.min(node.depth, MAX_INDENT);
    const indent = " ".repeat(visual * INDENT_COLS);
    const guide = node.depth > 0 ? "└ " : "";
    const overflow = node.depth > MAX_INDENT ? ` ·${node.depth}` : "";
    const marker = selectedMessageId === node.message.id ? "❯" : " ";
    const time = formatClock(node.message.ts);

    rows.push(
      pad(
        `${marker} ${indent}${guide}${truncateKeepingSuffix(
          `${node.message.author.name}${overflow}`,
          time,
          Math.max(1, cols - 2 - indent.length - guide.length),
        )}`,
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
      rows.push(pad(`  ${bodyIndent}${text}`, cols));
    });

    if (node.orphan) {
      // §3.2: "Orphan replies whose parent is not loaded render an explicit
      // backfill affordance rather than silently hiding."
      rows.push(pad(`  ${bodyIndent}┌ orphan — parent not loaded`, cols));
      rows.push(pad(`  ${bodyIndent}│  ^X b  backfill ancestors`, cols));
      rows.push(pad(`  ${bodyIndent}└`, cols));
    }
  }
  return rows;
}

/** `HH:MM` in UTC. See `layers/home.ts` for why UTC and not local. */
function formatClock(ts: number): string {
  const date = new Date(ts);
  return `${String(date.getUTCHours()).padStart(2, "0")}:${String(date.getUTCMinutes()).padStart(2, "0")}`;
}
