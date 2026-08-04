/**
 * The top rule, the composer, and the bottom rule — NAVIGATION.md §2, §2.2.
 *
 * ```
 * ┌───────────────────────────────────────────────────────────────────────── row 0
 * │  BODY                       flex — absorbs all residual height
 * ├─────────────────────────────────────────────────── breadcrumb ── TOP RULE (1)
 * │  ❯ composer / prompt                                             COMPOSER (1..n)
 * ├──────────────────────────────────────────────────────────────── BOTTOM RULE (1)
 * │  statusline (3)
 * └───────────────────────────────────────────────────────────────── row H-1
 * ```
 *
 * The breadcrumb is right-aligned **into the top rule**, costing zero rows —
 * §1.2's borrowing of Claude Code's session-label trick. At depth 5 that is the
 * difference between a spine you can read and one you have to remember.
 */

import { displayWidth, elideFromLeft, pad } from "./width";

/** The one focus glyph [G8]. There is exactly one on screen, ever. */
export const FOCUS_GLYPH = "❯";

/** The dim position marker a demoted row carries (§2.2). */
export const POSITION_GLYPH = "▌";

/**
 * The top rule with the breadcrumb right-aligned into it (§1.2).
 *
 * The crumb elides **from the left** — `… › #engineering › ⤷ read-state…` —
 * because the current location survives and the path is what is sacrificed
 * [G10]. Eliding from the right would produce a breadcrumb that tells you where
 * you started rather than where you are.
 */
export function topRule(crumb: string, cols: number): string {
  if (cols <= 0) return "";
  // ` <crumb> ` plus at least four rule cells on the left, so the rule still
  // reads as a rule rather than as a caption with a dash.
  const minRule = 4;
  const available = cols - minRule - 2;
  if (available <= 0) return "─".repeat(cols);
  const shown = elideFromLeft(crumb, available);
  const ruleWidth = cols - displayWidth(shown) - 3;
  return `${"─".repeat(Math.max(0, ruleWidth))} ${shown} ─`;
}

/** A plain full-width rule — the bottom rule above the statusline. */
export function rule(cols: number): string {
  return "─".repeat(Math.max(0, cols));
}

/** Composer render state. */
export interface ComposerView {
  /** Current text; empty renders the placeholder. */
  readonly text: string;
  readonly placeholder: string;
  /**
   * Whether the composer holds `❯` (§2.2).
   *
   * On chat layers this is true by default; on picker layers it becomes true
   * the moment one character is typed, and the list row demotes to `▌`. Derived
   * by the caller, never toggled — there is no focus key.
   */
  readonly focused: boolean;
}

/**
 * Render the composer row.
 *
 * A focused empty composer still shows `❯` followed by the placeholder: the
 * glyph marks *where keys go*, and on a chat layer that is the composer even
 * before you type. An unfocused composer renders its placeholder with **no
 * glyph** (§2.2), which is what keeps "exactly one `❯` on screen" true while
 * the list cursor holds it.
 */
export function renderComposer(view: ComposerView, cols: number): string[] {
  const body = view.text.length > 0 ? view.text : view.placeholder;
  const prefix = view.focused ? `${FOCUS_GLYPH} ` : "  ";
  return [pad(`${prefix}${body}`, cols)];
}

/** The fixed height of the rules. Used by the body's height arithmetic. */
export const RULE_ROWS = 1;
