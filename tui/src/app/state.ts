/**
 * The application state and its reducer — NAVIGATION.md §1–§5, end to end.
 *
 * One pure `(state, intent) → state` function. Nothing here renders and nothing
 * performs I/O, which is what makes the whole navigation model testable as a
 * table of key sequences: §4's four walkthroughs are literally four tests.
 *
 * The [G6] discipline shows up here as an absence: **there is no mode field.**
 * `drawer`, `messageSelect`, and `completion` are surfaces with data, and which
 * one interprets a key is decided by `nav/keys.ts` reading their presence, not
 * by a mode the user has to track. Adding a `mode: "drawer" | "chat" | …` would
 * make every key handler consult it and would reintroduce the invisible modal
 * state §3.7 calls "the number-one way a keybinding grammar feels broken".
 */

import type { Snapshot } from "../client/types";
import {
  type Layer,
  type NavStack,
  current,
  goHome,
  isChatLayer,
  newStack,
  pop,
  push,
  setSelection,
} from "../nav/layers";
import {
  type SurfaceSet,
  register,
  resolveEscape,
  unregister,
} from "../nav/surfaces";

/** Completion band kinds — `/ @ # : ctrl+f` (§2.5). */
export type CompletionKind = "slash" | "mention" | "channel" | "emoji" | "find";

/** The completion band's state, when one is open. */
export interface CompletionState {
  readonly kind: CompletionKind;
  /** Text between the trigger and the cursor. */
  readonly query: string;
  /** Grapheme offset of the trigger character in the composer text. */
  readonly triggerAt: number;
  readonly selection: number;
  /** True for `@@`, the agents-only trigger (DESIGN §3.3). */
  readonly agentsOnly: boolean;
}

/** The drawer's state, when open (§2.3, §2.4). */
export interface DrawerState {
  readonly selection: number;
  /** True once `⏎` has expanded the selected row in place (§2.4). */
  readonly expanded: boolean;
  /** Tail scroll offset from the newest line; 0 == pinned live. */
  readonly tailOffset: number;
}

/** Message-select state, when active (§3). */
export interface MessageSelectState {
  /** Index into the current layer's rendered message list, newest last. */
  readonly index: number;
}

/**
 * A saved draft: the text **and** its resolved mentions.
 *
 * The pair is the unit, not the text alone — see {@link swapDrafts}.
 */
export interface Draft {
  readonly text: string;
  readonly mentions: readonly string[];
}

/** Everything the app holds. */
export interface AppState {
  readonly stack: NavStack;
  readonly composer: string;
  readonly cursor: number;
  readonly surfaces: SurfaceSet;
  readonly drawer: DrawerState | null;
  readonly messageSelect: MessageSelectState | null;
  readonly completion: CompletionState | null;
  /**
   * Resolved pubkeys the composer will tag — [D-2].
   *
   * > The TUI sends resolved pubkeys, never names. […] "what you picked is what
   * > gets tagged" becomes true by construction rather than by two
   * > implementations agreeing.
   *
   * Accumulated at pick time, not re-extracted from the text at send time,
   * which is the whole point: re-extraction is the second implementation.
   */
  readonly mentions: readonly string[];
  /**
   * Per-layer drafts, keyed by a layer's identity.
   *
   * §5.4's gating table: "`ctrl+g` home with unsent composer text — draft is
   * persisted per layer, restored on return". Losing a half-written message to
   * a navigation key is the kind of small betrayal that makes people stop
   * trusting the arrows, which is the one thing this design cannot afford.
   */
  readonly drafts: ReadonlyMap<string, Draft>;
  /** Collapsed ATTENTION groups on home (§4.4). */
  readonly collapsedGroups: ReadonlySet<string>;
  /** Collapsed zones on home — `⏎` on `COMMUNITY` / `ATTENTION` / `PLACES`. */
  readonly collapsedZones: ReadonlySet<string>;
  /** The daemon snapshot the screens read. */
  readonly snapshot: Snapshot;
  /** Set when the last intent asked for an effect the reducer cannot perform. */
  readonly pending: PendingEffect | null;
}

/**
 * An effect the reducer *decided on* but cannot perform.
 *
 * The reducer stays pure; the shell drains this. Modelling effects as data
 * rather than as callbacks is what lets a test assert "pressing `⏎` with text
 * sends *this* to *that channel*" without a daemon.
 */
export type PendingEffect =
  | {
      kind: "send";
      channelId: string;
      content: string;
      replyTo?: string;
      /** Resolved pubkeys, never names — [D-2]. */
      mentions: readonly string[];
    }
  | { kind: "markRead"; channelId: string }
  | { kind: "markAllRead" };

/** A layer's draft key. Includes the ids, so two threads keep separate drafts. */
export function draftKey(layer: Layer): string {
  return [
    layer.kind,
    layer.channelId ?? "",
    layer.eventId ?? "",
    layer.agentPubkey ?? "",
  ].join(":");
}

/** The initial state for a snapshot. */
export function initialState(snapshot: Snapshot): AppState {
  return {
    stack: newStack(),
    composer: "",
    cursor: 0,
    surfaces: new Set(),
    drawer: null,
    messageSelect: null,
    completion: null,
    mentions: [],
    drafts: new Map(),
    collapsedGroups: new Set(),
    collapsedZones: new Set(),
    snapshot,
    pending: null,
  };
}

/**
 * Close every level-1 surface and restore the composer.
 *
 * Used by [G5]'s printable rule and by `Esc`. Both the drawer and
 * message-select are "transparent to typing" (§3), and they are transparent in
 * exactly the same way — so the restore is one function rather than two
 * near-identical branches that could drift.
 */
function restoreComposer(state: AppState): AppState {
  return {
    ...state,
    drawer: null,
    messageSelect: null,
    surfaces: unregister(unregister(state.surfaces, "drawer"), "messageSelect"),
  };
}

/** Swap the composer's text, keeping the cursor in range. */
function withComposer(
  state: AppState,
  text: string,
  cursor?: number,
): AppState {
  return { ...state, composer: text, cursor: cursor ?? text.length };
}

/**
 * Save the current layer's draft and switch to another layer's.
 *
 * Called on every push and pop, so a draft follows its layer rather than the
 * cursor — which is what makes `←` out of a thread and back into it feel like
 * returning to a place rather than to a cleared form.
 *
 * **The resolved mentions travel with the text**, because [D-2] makes them part
 * of the draft rather than metadata about it: restoring `hey @matt` with an
 * empty mention list would send a message whose visible `@matt` tags nobody,
 * which is exactly the "what you picked is what gets tagged" guarantee [D-2]
 * exists to make structural.
 */
function swapDrafts(state: AppState, from: Layer, to: Layer): AppState {
  const drafts = new Map(state.drafts);
  if (state.composer.length > 0) {
    drafts.set(draftKey(from), {
      text: state.composer,
      mentions: state.mentions,
    });
  } else {
    drafts.delete(draftKey(from));
  }
  const restored = drafts.get(draftKey(to));
  return {
    ...withComposer(state, restored?.text ?? ""),
    mentions: restored?.mentions ?? [],
    drafts,
  };
}

/**
 * Descend into a layer — the `→` verb (§1.1).
 *
 * Before pushing, the **message-select index is written into the layer's
 * `selection`**. That is what makes §4.2's last step true:
 *
 * > `←` returns to L2 **with the same message still selected**.
 *
 * Without it the index would live only in `state.messageSelect`, which the
 * descent clears — and returning would land on the newest message rather than
 * on the one you opened a thread from, which is precisely the "the layer you
 * return to is the layer you left, not its first row" failure §1.1 names.
 */
export function descend(state: AppState, layer: Layer): AppState {
  const from = current(state.stack);
  const remembered = state.messageSelect
    ? setSelection(state.stack, state.messageSelect.index)
    : state.stack;
  const withDraft = swapDrafts({ ...state, stack: remembered }, from, layer);
  return {
    ...restoreComposer(withDraft),
    stack: push(withDraft.stack, layer),
    completion: null,
    surfaces: unregister(
      unregister(unregister(state.surfaces, "drawer"), "messageSelect"),
      "completion",
    ),
  };
}

/**
 * Ascend one layer — the `←` verb (§1.1).
 *
 * Restores message-select when returning to a chat layer that had one, which is
 * the other half of the §4.2 property {@link descend} sets up. §4.2 then adds
 * the escape hatch: "A printable key at that point dismisses selection and
 * starts a channel message instead [G5] — no keystroke lost either way."
 *
 * At L0 this returns the state **unchanged**, including the composer: a `←`
 * that cleared a draft at home would be a dismissal, and §1.1 is explicit that
 * `←` never dismisses.
 */
export function ascend(state: AppState): AppState {
  if (state.stack.entries.length <= 1) return state;
  const from = current(state.stack);
  const nextStack = pop(state.stack);
  const to = current(nextStack);
  const withDraft = swapDrafts(state, from, to);
  const restored = {
    ...restoreComposer(withDraft),
    stack: nextStack,
    completion: null,
  };
  if (!isChatLayer(to.kind)) return restored;
  return {
    ...restored,
    messageSelect: { index: to.selection },
    surfaces: register(restored.surfaces, "messageSelect"),
  };
}

/** `ctrl+g` — collapse to L0, persisting the draft (§5.4). */
export function collapseHome(state: AppState): AppState {
  const from = current(state.stack);
  const nextStack = goHome(state.stack);
  const to = current(nextStack);
  const withDraft = swapDrafts(state, from, to);
  return { ...restoreComposer(withDraft), stack: nextStack, completion: null };
}

/** Open the drawer — `↓` from an empty composer on a chat layer (§2.3). */
export function openDrawer(state: AppState): AppState {
  if (!isChatLayer(current(state.stack).kind)) return state;
  return {
    ...state,
    drawer: { selection: 0, expanded: false, tailOffset: 0 },
    surfaces: register(state.surfaces, "drawer"),
  };
}

/** Enter message-select — `↑` from an empty composer on a chat layer (§3). */
export function enterMessageSelect(
  state: AppState,
  newestIndex: number,
): AppState {
  if (!isChatLayer(current(state.stack).kind)) return state;
  return {
    ...state,
    messageSelect: { index: newestIndex },
    surfaces: register(state.surfaces, "messageSelect"),
  };
}

/**
 * `⏎` on a drawer row — peek, expanding in place (§2.4).
 *
 * Also the `←` verb *inside* an expansion, which pops back to the list. Both
 * are the same toggle, so they cannot drift apart.
 */
export function toggleDrawerExpansion(state: AppState): AppState {
  if (!state.drawer) return state;
  const expanded = !state.drawer.expanded;
  return {
    ...state,
    drawer: { ...state.drawer, expanded, tailOffset: 0 },
    surfaces: expanded
      ? register(state.surfaces, "drawerExpansion")
      : unregister(state.surfaces, "drawerExpansion"),
  };
}

/**
 * Apply one `Esc` press — §5.3's three tiers.
 *
 * Tier 3 returns a `markRead` effect only when nothing is registered, which is
 * §5.4's gate. That suppression is why walking out of a drawer expansion takes
 * two presses and neither of them silently marks a channel read.
 */
export function applyEscape(state: AppState): AppState {
  const resolution = resolveEscape(state.surfaces);
  switch (resolution.tier) {
    case 1:
      return {
        ...state,
        completion: null,
        surfaces: unregister(state.surfaces, "completion"),
      };
    case 2:
      switch (resolution.close) {
        case "drawerExpansion":
          return toggleDrawerExpansion(state);
        case "drawer":
          return {
            ...state,
            drawer: null,
            surfaces: unregister(state.surfaces, "drawer"),
          };
        case "messageSelect":
          return {
            ...state,
            messageSelect: null,
            surfaces: unregister(state.surfaces, "messageSelect"),
          };
        default:
          return {
            ...state,
            surfaces: unregister(state.surfaces, resolution.close),
          };
      }
    case 3: {
      const layer = current(state.stack);
      if (!layer.channelId) return state;
      return {
        ...state,
        pending: { kind: "markRead", channelId: layer.channelId },
      };
    }
  }
}

/**
 * Insert a printable character — [G5], §2.4's most important row.
 *
 * > at list level: **close, restore composer, insert the character**.
 *
 * The close and the insert happen in **one** keystroke, not two. A version that
 * only closed would make `↓` a gesture you have to undo before typing, which is
 * precisely the risk §2.4 says this rule removes.
 */
export function insertChar(state: AppState, char: string): AppState {
  const restored = restoreComposer(state);
  const text =
    restored.composer.slice(0, restored.cursor) +
    char +
    restored.composer.slice(restored.cursor);
  return { ...withComposer(restored, text, restored.cursor + char.length) };
}

/**
 * Clear the composer after a send, dropping the layer's saved draft **and its
 * resolved mentions** with it.
 *
 * Clearing the text while keeping the mentions would carry the previous
 * message's `p` tags onto the next one — a silent mis-tag that neither the
 * composer nor the sent message would show.
 */
export function clearComposer(state: AppState): AppState {
  const drafts = new Map(state.drafts);
  drafts.delete(draftKey(current(state.stack)));
  return { ...withComposer(state, ""), mentions: [], drafts };
}

/** Record the current layer's selection so `←` can restore it (§1.1). */
export function selectRow(state: AppState, index: number): AppState {
  return { ...state, stack: setSelection(state.stack, index) };
}

/** Drain the pending effect, returning it and the cleared state. */
export function takeEffect(state: AppState): [PendingEffect | null, AppState] {
  if (!state.pending) return [null, state];
  return [state.pending, { ...state, pending: null }];
}
