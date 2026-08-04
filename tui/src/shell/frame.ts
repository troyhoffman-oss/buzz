/**
 * Frame assembly — NAVIGATION.md §2's band stack, as a pure function.
 *
 * ```
 * │  BODY                       flex — absorbs all residual height
 * ├─────────────────────────────────── breadcrumb ── TOP RULE (1)
 * │  ❯ composer                                      COMPOSER (1..n)
 * ├──────────────────────────────────────────────── BOTTOM RULE (1)
 * │  statusline                                      STATUSLINE (3)
 * ```
 *
 * The whole screen is computed as `string[]` here and handed to OpenTUI as
 * text. That is deliberate and it is what makes the T1 matrix meaningful: a
 * snapshot compares the same array the terminal receives, so "renders
 * identically at 120 and 60 columns" is a statement about this function, not
 * about a component tree that might lay out differently under a different flex
 * solver.
 *
 * The **drawer and message-select replace the composer and statusline** (§2.3,
 * §3) rather than overlaying them. Nothing in this design opens downward,
 * nothing floats, nothing is centered [G1] — and the arithmetic below is where
 * that becomes true: the body's height is whatever the bottom bands leave, so a
 * taller bottom band compresses the chat and can never cover it [G16].
 */

import { RULE_ROWS, renderComposer, rule, topRule } from "../render/bands";
import { STATUSLINE_ROWS } from "../render/statusline";
import { pad } from "../render/width";

/** The bottom region: what occupies the rows below the top rule. */
export type BottomBand =
  | { kind: "composer"; text: string; placeholder: string; focused: boolean }
  /** The drawer or message-select — both replace composer + statusline (§2.3, §3). */
  | { kind: "replaced"; rows: readonly string[] };

/** Everything the frame needs. */
export interface FrameInput {
  readonly cols: number;
  readonly rows: number;
  /** Body rows, newest last. Anchored to the bottom for chat, top for lists. */
  readonly body: readonly string[];
  /** Which end of the body survives when it overflows. */
  readonly bodyAnchor: "top" | "bottom";
  readonly crumb: string;
  readonly bottom: BottomBand;
  /** Statusline rows. Ignored when the bottom band is `replaced`. */
  readonly statusline: readonly string[];
  /** Completion band rows, rendered **above the top rule** (§2.5). */
  readonly completion?: readonly string[];
  /**
   * Body row index the viewport must keep visible.
   *
   * The selection has to follow the cursor off the visible window, or `↑` in a
   * 400-message channel walks the selection into scrollback and the `❯`
   * disappears — a selection you cannot see is [G8] broken in the way that
   * matters, since the glyph's entire job is to mark where keys go. Absent
   * means "no selection to follow", which is the composer-resident case.
   */
  readonly follow?: number;
}

/**
 * Assemble one frame.
 *
 * Always returns exactly `rows` rows of exactly `cols` columns. Anything less
 * would let a previous frame's content survive at that position, which on a
 * streaming chat surface reads as a rendering glitch rather than as a bug.
 */
export function renderFrame(input: FrameInput): string[] {
  const { cols, rows, crumb } = input;
  if (cols <= 0 || rows <= 0) return [];

  const bottomRows =
    input.bottom.kind === "replaced"
      ? [...input.bottom.rows]
      : [
          ...renderComposer(
            {
              text: input.bottom.text,
              placeholder: input.bottom.placeholder,
              focused: input.bottom.focused,
            },
            cols,
          ),
          rule(cols),
          ...input.statusline.map((r) => pad(r, cols)),
        ];

  const completion = (input.completion ?? []).map((r) => pad(r, cols));

  // Top rule + completion band + bottom band is fixed overhead; the body gets
  // the rest. Floored at zero rather than clamped to one: below the §3.9 floor
  // the caller renders the one-line fallback instead, and a frame that silently
  // stole a row from the bottom band would hide the composer rather than the
  // body — exactly backwards.
  const overhead = RULE_ROWS + completion.length + bottomRows.length;
  const bodyRows = Math.max(0, rows - overhead);

  const body = input.body.map((r) => pad(r, cols));
  const blanks = (n: number): string[] =>
    Array.from({ length: n }, () => pad("", cols));

  let shown: string[];
  if (body.length <= bodyRows) {
    // Chat sticks to the bottom: a half-empty channel shows its messages above
    // the composer, not floating at the top of the screen. Lists stick to the
    // top, where a list starts.
    shown =
      input.bodyAnchor === "bottom"
        ? [...blanks(bodyRows - body.length), ...body]
        : [...body, ...blanks(bodyRows - body.length)];
  } else {
    // The window's default end: the newest row for chat, the first for a list.
    let end = input.bodyAnchor === "bottom" ? body.length : bodyRows;
    if (input.follow !== undefined) {
      // Scroll just far enough to bring the followed row inside the window.
      // Minimum movement rather than centering: on a chat surface, re-centering
      // on every `↑` makes the whole timeline slide under the cursor, and the
      // messages around the selection are the context you are reading it in.
      if (input.follow >= end) end = input.follow + 1;
      else if (input.follow < end - bodyRows) end = input.follow + bodyRows;
    }
    end = Math.max(bodyRows, Math.min(body.length, end));
    shown = body.slice(end - bodyRows, end);
  }

  const out = [
    ...shown,
    pad(topRule(crumb, cols), cols),
    ...completion,
    ...bottomRows,
  ];

  // The bottom band may be taller than the terminal on a very short one. Keep
  // the *end* — the composer and the hint footer are the exits, and losing them
  // is what [G14] forbids.
  return out.length <= rows ? out : out.slice(out.length - rows);
}

/** Rows the fixed bands occupy when the composer is showing. */
export const COMPOSER_BAND_ROWS = RULE_ROWS + 1 + RULE_ROWS + STATUSLINE_ROWS;
