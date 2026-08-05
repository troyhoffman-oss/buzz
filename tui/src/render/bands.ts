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

import { CHROME, FOCUS, META, PLACEHOLDER } from "./palette";
import { type StyledRow, padRow, plain, styled } from "./span";
import { displayWidth, elideFromLeft } from "./width";

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
 *
 * The rule cells and the crumb are **separate spans**, and that is the whole
 * visual point of the band: the rule recedes to `borderSubtle` while the crumb
 * sits a step brighter at `textMuted`, so "where am I" is legible at a glance
 * without the rule competing with it. Drawing both in one colour is what made
 * the M3 frames read as a wall — the breadcrumb was the same weight as eighty
 * dashes beside it.
 */
export function topRule(crumb: string, cols: number): StyledRow {
  if (cols <= 0) return [];
  // ` <crumb> ` plus at least four rule cells on the left, so the rule still
  // reads as a rule rather than as a caption with a dash.
  const minRule = 4;
  const available = cols - minRule - 2;
  if (available <= 0) return [styled("─".repeat(cols), CHROME)];
  const shown = elideFromLeft(crumb, available);
  const ruleWidth = cols - displayWidth(shown) - 3;
  return [
    styled("─".repeat(Math.max(0, ruleWidth)), CHROME),
    plain(" "),
    styled(shown, META),
    plain(" "),
    styled("─", CHROME),
  ];
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
 *
 * # The three states, and why they are three colours
 *
 * The M3 captures rendered all three identically — `❯ message DM` and
 * `  filter channels` and a half-typed message were the same grey text — so the
 * composer could not tell you whether it was listening, and typed text looked
 * exactly like the hint it replaced. Claude Code's input line answers this by
 * having no placeholder at all (`nav/cc-research.md` §1.3: a bare `❯` with
 * nothing after it), which it can afford because it has one input mode. Buzz's
 * composer changes target per layer — `message #engineering`, `reply in
 * thread`, `steer claude-1` — so the placeholder is carrying real information
 * and deleting it would cost more than it saves.
 *
 * The resolution is to keep the placeholder and make it unmistakably *not*
 * text: it renders in `textMuted`, while typed text renders at full `text`
 * weight. Combined with the accent `❯`, the three states read at a glance:
 *
 * - **focused, empty** — bright `❯`, receded placeholder: "type here, here is
 *   what this line does".
 * - **focused, typed** — bright `❯`, full-weight text: "this is yours".
 * - **unfocused** — no glyph, receded placeholder: "keys are going somewhere
 *   else" (§2.2's list-owns-the-cursor case).
 */
export function renderComposer(view: ComposerView, cols: number): StyledRow[] {
  const prefix = view.focused ? styled(`${FOCUS_GLYPH} `, FOCUS) : plain("  ");
  const body =
    view.text.length > 0
      ? plain(view.text)
      : styled(view.placeholder, PLACEHOLDER);
  // Padded to the full width like every other band. An unpadded row lets the
  // previous frame's tail survive to the right of the composer — on a
  // streaming chat surface that reads as a rendering glitch rather than as a
  // bug, which is why `renderFrame` promises exactly `cols` columns on every
  // row and why the reflow suite asserts it.
  return [padRow([prefix, body], cols)];
}

/** The fixed height of the rules. Used by the body's height arithmetic. */
export const RULE_ROWS = 1;
