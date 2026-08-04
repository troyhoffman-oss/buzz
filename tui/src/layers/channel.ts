/**
 * L2 CHANNEL — NAVIGATION.md §3, §4.1, §4.2.
 *
 * The 95% case. Full-screen timeline, composer resident, and the two level-1
 * surfaces reachable from an empty composer: `↑` into the conversation, `↓`
 * into what is running.
 *
 * §3's symmetry is the design's centre of gravity:
 *
 * > - `↑` = go up **the conversation** → message-select
 * > - `↓` = go down into **what is running** → drawer
 * >
 * > Both are level-1 surfaces: both **replace the composer + statusline** with
 * > a hint footer while active, both are transparent to typing [G5], both are
 * > `Esc`-reversible, and **neither is a mode** — each is the fallback
 * > interpretation of its arrow when the composer is empty [G6].
 */

import type { Message } from "../client/types";
import type { Layer } from "../nav/layers";
import { clampSelection } from "../nav/keys";
import { selectableIds, threadRootIds } from "../render/timeline";
import { pad, truncate, wrapHints } from "../render/width";

/** Message-select's hint footer, replacing the statusline while active (§4.2). */
export const MESSAGE_SELECT_HINTS = [
  "↑/↓ message",
  "⇧↑/⇧↓ thread",
  "→ open",
  "esc back",
] as const;

/**
 * Render the hint footer. Wraps, never truncates [G14].
 *
 * Padded to the full width like every other band: an unpadded row lets the
 * previous frame's content survive at that position, and this band replaces the
 * statusline — which is wider than it is.
 */
export function messageSelectHints(cols: number): string[] {
  return wrapHints([...MESSAGE_SELECT_HINTS], Math.max(1, cols - 2)).map((h) =>
    pad(`  ${h}`, cols),
  );
}

/**
 * Where message-select starts — the newest message (§3's table).
 *
 * > `↑`/`↓` step one message, clamped, **starting from the newest**.
 *
 * Starting at the newest is what makes a single `↑` useful: the message you
 * most likely want to act on is the one that just arrived.
 */
export function initialSelectIndex(messages: readonly Message[]): number {
  return Math.max(0, selectableIds(messages).length - 1);
}

/** Step the message-select cursor one message, clamped, no wrap [G4]. */
export function stepSelection(
  messages: readonly Message[],
  index: number,
  delta: number,
): number {
  return clampSelection(index + delta, selectableIds(messages).length);
}

/**
 * Step **thread root to thread root** — `⇧↑`/`⇧↓` (§3, §4.2).
 *
 * Lands on the nearest root strictly past the current position in the given
 * direction, and clamps at the ends rather than wrapping. When the cursor is
 * already past the last root in that direction it stays put: §1.1's "boundaries
 * are never trapdoors" means a `⇧↑` at the oldest thread does not jump to the
 * newest.
 */
export function stepThreadRoot(
  messages: readonly Message[],
  index: number,
  delta: number,
): number {
  const order = selectableIds(messages);
  const roots = new Set(threadRootIds(messages));
  const step = delta < 0 ? -1 : 1;
  for (let i = index + step; i >= 0 && i < order.length; i += step) {
    const id = order[i];
    if (id !== undefined && roots.has(id)) return i;
  }
  return index;
}

/** The message the cursor is on, or `undefined` in an empty channel. */
export function selectedMessage(
  messages: readonly Message[],
  index: number,
): Message | undefined {
  const id = selectableIds(messages)[index];
  if (id === undefined) return undefined;
  return messages.find((m) => m.id === id);
}

/**
 * Where `→`/`⏎` from message-select descends to — L3 THREAD (§3, §4.2).
 *
 * §8 ruling 2 is what makes this the *only* way into a thread from the
 * timeline:
 *
 * > message-select is the entry to thread descent: select a message with
 * > replies, `→` descends into its thread; shift-up/down leaps thread roots.
 *
 * A message with no replies still descends — the thread is simply empty, and
 * replying there is how a thread starts. Refusing would make the key
 * conditionally dead, and a key that sometimes does nothing is worse than one
 * that opens an empty room.
 */
export function threadTarget(message: Message): Layer {
  // The crumb is the thread's first line, which is what §1.2's frame shows:
  // `home › channels › #engineering › ⤷ read-state slots`. The channel name is
  // deliberately *not* repeated here — it is already the previous crumb, and
  // duplicating it would eat the width §1.2's left-elision is trying to save.
  const title = message.content.split("\n")[0] ?? "";
  return {
    kind: "thread",
    channelId: message.channelId,
    eventId: message.id,
    crumb: `⤷ ${truncate(title, THREAD_CRUMB_COLS)}`,
    selection: 0,
  };
}

/**
 * How much of a thread's first line the crumb carries.
 *
 * Enough to identify the thread among the handful you have open, short enough
 * that a five-deep breadcrumb still fits before §1.2's elision kicks in.
 */
const THREAD_CRUMB_COLS = 24;

// `channelScope(layer) => layer.crumb` used to live here, "so the shell has one
// source" for the label. It never had a second caller: `screen.ts` reads
// `layer.crumb` for both the breadcrumb and the statusline scope, so the crumb
// *is* the single source and the wrapper only added a name to look through.
