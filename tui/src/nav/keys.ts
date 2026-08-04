/**
 * The key priority chains — NAVIGATION.md §5.
 *
 * > **[G6] Keys resolve through a state priority chain, not modes.** The most
 * > *local* interpretation wins; navigation is the fallback that fires only
 * > when the local one is vacuous.
 *
 * Everything in this module is a **pure function of state → intent**. Nothing
 * here renders, mutates, or performs I/O, which is what makes §5's chains
 * testable as tables rather than as a driven UI. The reducer in `app.ts` is the
 * only place an intent turns into a state change.
 */

import { type LayerKind, isChatLayer } from "./layers";
import { type SurfaceSet, resolveEscape } from "./surfaces";

/** A key press, normalized from the terminal's event. */
export interface KeyPress {
  /** Key name: `up`, `down`, `left`, `right`, `return`, `escape`, or a char. */
  readonly name: string;
  readonly shift?: boolean;
  readonly ctrl?: boolean;
  readonly meta?: boolean;
  /** The printable character this press produces, when it produces one. */
  readonly char?: string;
}

/** The composer's text state, as far as key resolution is concerned. */
export interface ComposerState {
  readonly text: string;
  /** Cursor offset in grapheme clusters. */
  readonly cursor: number;
  /** True when the text occupies more than one rendered row. */
  readonly multiline: boolean;
  /** Rendered row the cursor sits on, and the total; for the §5.2 chain. */
  readonly cursorRow: number;
  readonly rowCount: number;
}

/** Everything §5's chains read. Assembled by the caller, never mutated here. */
export interface KeyContext {
  readonly layer: LayerKind;
  readonly composer: ComposerState;
  readonly surfaces: SurfaceSet;
  /** True when the drawer list (not its expansion) has the selection. */
  readonly drawerOpen: boolean;
  /** True when the drawer's in-place expansion is showing (§2.4). */
  readonly drawerExpanded: boolean;
  /** True when message-select is active (§3). */
  readonly messageSelect: boolean;
  /** True when a group header is the selected row (§5.1 row 3). */
  readonly groupHeaderSelected: boolean;
  /** True when the selected row is an L0 ATTENTION row (§5.1 row 5). */
  readonly attentionRowSelected: boolean;
}

/** The resolved meaning of a key press. */
export type Intent =
  | { kind: "completionAccept" }
  | { kind: "completionMove"; delta: number }
  | { kind: "completionClose" }
  | { kind: "send" }
  | { kind: "toggleGroup" }
  | { kind: "peek" }
  | { kind: "openThread" }
  | { kind: "descend" }
  | { kind: "ascend" }
  | { kind: "moveSelection"; delta: number }
  | { kind: "jumpStructural"; delta: number }
  | { kind: "moveTextCursor"; delta: number }
  | { kind: "moveTextEdge"; edge: "start" | "end" }
  | { kind: "openDrawer" }
  | { kind: "enterMessageSelect" }
  | { kind: "scrollTail"; delta: number }
  | { kind: "escape" }
  | { kind: "insertChar"; char: string }
  | { kind: "goHome" }
  | { kind: "openSearch" }
  | { kind: "openPalette" }
  | { kind: "findInChannel" }
  | { kind: "unreadJump"; delta: number }
  | { kind: "markAllRead" }
  | { kind: "none" };

/** Whether the completion band is open (§2.5). */
function completionOpen(ctx: KeyContext): boolean {
  return ctx.surfaces.has("completion");
}

/**
 * `⏎` — §5.1's chain, verbatim.
 *
 * ```
 * 1. completion band open                    → accept selection
 * 2. composer has text                       → send to the layer's target (§2.2)
 * 3. group header selected                   → collapse / expand the group
 * 4. drawer open, row selected               → PEEK: expand in place
 * 5. L0 attention row selected               → PEEK: expand in place
 * 6. message-select active                   → open thread (no peek presentation)
 * 7. list focus at any other layer           → descend (identical to →)
 * ```
 *
 * Rows 4–5 are the [G9] middle verb: `⏎` peeks, `→` commits. Where a row has no
 * peek presentation (rows 6–7) the two keys are the same, so **`⏎` is never
 * wrong** — it is either the cheaper of two moves or the only one.
 *
 * Row 2 preceding rows 3–7 is what makes §4.1's last step work: you type into
 * the composer and press `⏎`, and the descend interpretation never fires.
 */
export function resolveEnter(ctx: KeyContext): Intent {
  if (completionOpen(ctx)) return { kind: "completionAccept" };
  if (ctx.composer.text.length > 0) return { kind: "send" };
  if (ctx.groupHeaderSelected) return { kind: "toggleGroup" };
  if (ctx.drawerOpen || ctx.drawerExpanded) return { kind: "peek" };
  if (ctx.attentionRowSelected) return { kind: "peek" };
  if (ctx.messageSelect) return { kind: "openThread" };
  return { kind: "descend" };
}

/**
 * `↑` / `↓` — §5.2's chain, verbatim.
 *
 * ```
 * ├─ completion band open                      → move selection
 * ├─ composer multiline, cursor has a row to   → move text cursor
 * │    move to in that direction
 * ├─ composer has text (single line)           → move to start / end of input
 * ├─ drawer or message-select active           → move selection (clamped, no wrap)
 * ├─ PICKER LAYER (L0, L1) — list owns ❯       → move list selection (clamped)
 * └─ CHAT LAYER (L2, L3, L4), composer empty   → ↑ ENTER MESSAGE-SELECT
 *                                                ↓ OPEN DRAWER
 * ```
 *
 * > Arrows **type** whenever there is text under the cursor to move through,
 * > and **navigate** only when that interpretation is vacuous. The user never
 * > loses a keystroke to the wrong handler and never has to ask what mode they
 * > are in.
 *
 * The "has a row to move to" qualifier on the multiline branch is not a detail:
 * without it, `↑` on the first row of a multiline draft would move the cursor
 * nowhere and swallow the press, so the user would learn that `↑` sometimes
 * does nothing. With it, that press falls through to the next interpretation.
 */
export function resolveVertical(ctx: KeyContext, delta: -1 | 1): Intent {
  if (completionOpen(ctx)) return { kind: "completionMove", delta };

  if (ctx.composer.multiline) {
    const target = ctx.composer.cursorRow + delta;
    if (target >= 0 && target < ctx.composer.rowCount) {
      return { kind: "moveTextCursor", delta };
    }
  }
  if (ctx.composer.text.length > 0) {
    return { kind: "moveTextEdge", edge: delta < 0 ? "start" : "end" };
  }

  // In the expanded drawer the arrows scroll the fixed-height tail window
  // (§2.4: "in expanded view, scroll the tail"); the box does not grow.
  if (ctx.drawerExpanded) return { kind: "scrollTail", delta };
  if (ctx.drawerOpen || ctx.messageSelect)
    return { kind: "moveSelection", delta };

  if (!isChatLayer(ctx.layer)) return { kind: "moveSelection", delta };

  return delta < 0 ? { kind: "enterMessageSelect" } : { kind: "openDrawer" };
}

/**
 * `⇧↑` / `⇧↓` — jump by **structural unit** (§1.1).
 *
 * > one gesture, one meaning: skip everything that is not a significant
 * > boundary — group header in a list, thread root in a timeline (§3).
 *
 * In message-select this is the answer to "pick a thread in a busy channel":
 * six presses reach the oldest live thread and nothing else is ever selected on
 * the way (§4.2).
 */
export function resolveShiftVertical(ctx: KeyContext, delta: -1 | 1): Intent {
  if (completionOpen(ctx)) return { kind: "completionMove", delta };
  if (ctx.composer.text.length > 0) return { kind: "none" };
  return { kind: "jumpStructural", delta };
}

/**
 * `→` — **commit** (§1.1).
 *
 * > the only travel verb between layers. Unified across every surface.
 *
 * Inside the composer `→` is a text cursor move: the composer owns the key
 * whenever there is text to the right of the cursor, exactly as the vertical
 * chain gives the composer `↑`/`↓` while there is a row to move to. Otherwise
 * it descends — including from the drawer, where §2.4 makes it the promote verb
 * and §5.4 notes the drawer stays warm so `←` returns.
 */
export function resolveRight(ctx: KeyContext): Intent {
  if (completionOpen(ctx)) return { kind: "completionAccept" };
  if (ctx.composer.cursor < ctx.composer.text.length) {
    return { kind: "moveTextCursor", delta: 1 };
  }
  return { kind: "descend" };
}

/**
 * `←` — **ascend** (§1.1).
 *
 * > never dismisses; at L0 it is a **no-op** [G3].
 *
 * `←` is excluded from all three `Esc` tiers (§5.3) — it is a pure depth verb,
 * safe to hold. The one nuance §2.4 adds: inside the drawer, `←` pops an
 * expansion back to the list and is a no-op at list level, so it never closes
 * the drawer. Closing is `Esc`'s job, and keeping the two verbs disjoint is
 * what lets a user hold `←` to walk out of a deep spine without wondering what
 * they dismissed on the way.
 */
export function resolveLeft(ctx: KeyContext): Intent {
  if (completionOpen(ctx)) return { kind: "completionClose" };
  if (ctx.composer.cursor > 0) return { kind: "moveTextCursor", delta: -1 };
  if (ctx.drawerExpanded) return { kind: "peek" };
  if (ctx.drawerOpen) return { kind: "none" };
  return { kind: "ascend" };
}

/**
 * A printable character — §5's [G5] rule, and §2.4's most important row.
 *
 * > printable — at list level: **close, restore composer, insert the
 * > character**. At expanded level: captured.
 * >
 * > That last row is what makes `↓` a zero-risk gesture in a chat-first app.
 * > You never have to think about focus before typing, so long as you have not
 * > deliberately descended. It is the single most important behavior to port.
 *
 * The same holds for message-select (§3's table: "printable — dismiss, restore
 * composer, insert the character") — the two level-1 surfaces are symmetric,
 * and both are transparent to typing.
 */
export function resolvePrintable(ctx: KeyContext, char: string): Intent {
  if (ctx.drawerExpanded) return { kind: "none" };
  return { kind: "insertChar", char };
}

/**
 * The direct shortcuts of §1.4.
 *
 * > Deliberately few. Every entry is chorded — **no bare letter is bound
 * > anywhere outside a modal detail view**, because bare letters must stay
 * > available to the composer [G5].
 *
 * Checked before the chains, since a chord cannot be a composer keystroke and
 * therefore has no more local interpretation to lose to.
 */
export function resolveShortcut(key: KeyPress): Intent | null {
  if (key.ctrl) {
    switch (key.name) {
      case "k":
        return { kind: "openSearch" };
      case "p":
        return { kind: "openPalette" };
      case "g":
        return { kind: "goHome" };
      case "f":
        return { kind: "findInChannel" };
      case "up":
        return { kind: "unreadJump", delta: -1 };
      case "down":
        return { kind: "unreadJump", delta: 1 };
      default:
        return null;
    }
  }
  if (key.shift && key.name === "escape") return { kind: "markAllRead" };
  return null;
}

/**
 * Resolve one key press against the full context — the single entry point.
 *
 * Order: shortcuts (§1.4, chorded and therefore unambiguous), then `Esc`
 * (§5.3), then the arrow and `⏎` chains (§5.1, §5.2), then printable [G5].
 */
export function resolveKey(ctx: KeyContext, key: KeyPress): Intent {
  const shortcut = resolveShortcut(key);
  if (shortcut) return shortcut;

  switch (key.name) {
    case "escape":
      return { kind: "escape" };
    case "return":
      return resolveEnter(ctx);
    case "up":
      return key.shift
        ? resolveShiftVertical(ctx, -1)
        : resolveVertical(ctx, -1);
    case "down":
      return key.shift ? resolveShiftVertical(ctx, 1) : resolveVertical(ctx, 1);
    case "left":
      return resolveLeft(ctx);
    case "right":
      return resolveRight(ctx);
    default:
      if (key.char && key.char.length > 0 && !key.ctrl && !key.meta) {
        return resolvePrintable(ctx, key.char);
      }
      return { kind: "none" };
  }
}

/** Re-exported so callers resolving `Esc` do not need two imports. */
export { resolveEscape };

/**
 * Clamp a selection index — [G4], "clamped, no wrap".
 *
 * > boundaries are never trapdoors; `↑` at the top does not exit.
 *
 * Wrapping is the trapdoor this forbids: at the top of the ATTENTION zone, a
 * wrap would drop you at the bottom of PLACES, which reads as a teleport you
 * did not ask for.
 */
export function clampSelection(index: number, count: number): number {
  if (count <= 0) return 0;
  return Math.max(0, Math.min(count - 1, index));
}
