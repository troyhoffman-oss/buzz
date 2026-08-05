/**
 * Styled rows — the contract that lets DESIGN.md §3.10 reach the screen.
 *
 * Wave 1 shipped every renderer as `string → string[]` and the Shell painted
 * the whole frame in one `(fg, bg)` pair, with a TODO saying per-token colour
 * "needs the renderers to emit *spans* rather than plain strings, which is a
 * change to the `string[]` contract `renderScreen` and the whole T1 matrix are
 * built on". This module is that change, made without breaking the contract.
 *
 * # Why spans rather than a post-hoc styling pass
 *
 * The tempting shortcut is to leave the renderers alone and colour the finished
 * rows by matching them — "a row starting with `❯` is selected", "a row of `─`
 * is a rule". That is re-deriving structure from its own rendering, and it is
 * wrong in the way that costs the most later: the renderer already *knows* which
 * part is the marker and which is the label, and a matcher that re-guesses it
 * silently mis-colours the first row whose content happens to look like chrome —
 * a message body beginning with `❯`, a channel literally named `─`. The
 * knowledge exists at the point of construction and this module keeps it there.
 *
 * # Why `string[]` survives
 *
 * A {@link StyledRow} is a list of spans whose concatenated text **is** the
 * string the renderer would have produced. So `rowText` is a total projection
 * back to the old contract, and `renderScreen` keeps its `string[]` signature
 * by applying it. Every geometry assertion in the T1 matrix, the walkthroughs
 * and the empty-state suite keeps testing exactly what it tested before —
 * which is what makes this safe to land in one pass rather than as a fork of
 * the render path that would have to be kept in agreement with the real one.
 *
 * The invariant is mechanical, not aspirational: {@link padRow} is the only way
 * a row reaches its final width, and it applies the same `clip`/`pad` the
 * string path applied, so `rowText(padRow(spans, cols)) === pad(text, cols)`
 * holds by construction. `test/unit/span.test.ts` pins it.
 */

import type { TokenName } from "../theme/tokens";
import { clip, clusterWidth, displayWidth, graphemes } from "./width";

/**
 * How a span is drawn.
 *
 * Colour is a **token name**, never a colour: §3.10's "no literal hex in
 * feature code" is enforced by `check-boundary.sh`, and naming the token at the
 * construction site keeps the palette the only place a hex exists.
 *
 * There is deliberately no `dim` flag. ANSI dim is a terminal-dependent
 * intensity hint that some emulators ignore and others render as a different
 * hue entirely, and §3.10 already supplies the recession this design wants as
 * *colours* — `textMuted` and `borderSubtle` are the muted ramp, and they are
 * the same on every terminal that can draw 24-bit colour. Recession is a
 * palette decision here, not a terminal capability.
 */
export interface SpanStyle {
  /** Foreground token. Absent means the frame's base text colour. */
  readonly fg?: TokenName;
  /** Background token. Absent means the frame's base background. */
  readonly bg?: TokenName;
  readonly bold?: boolean;
  readonly italic?: boolean;
  readonly underline?: boolean;
}

/** A run of text drawn in one style. */
export interface Span extends SpanStyle {
  readonly text: string;
}

/** One screen row: spans left to right, concatenating to the row's text. */
export type StyledRow = readonly Span[];

/** A span with no styling — the frame's base text colour. */
export function plain(text: string): Span {
  return { text };
}

/** A styled span. */
export function styled(text: string, style: SpanStyle): Span {
  return { text, ...style };
}

/** The text a styled row renders to — the projection back to `string[]`. */
export function rowText(row: StyledRow): string {
  let out = "";
  for (const span of row) out += span.text;
  return out;
}

/** A whole row in one style. */
export function styledRow(text: string, style: SpanStyle): StyledRow {
  return [styled(text, style)];
}

/** A whole row, unstyled. */
export function plainRow(text: string): StyledRow {
  return [plain(text)];
}

/**
 * Clip and pad a styled row to exactly `cols` columns.
 *
 * The string path's `pad` is `clip` then space-fill, and this reproduces both
 * across the span boundary: spans are taken until the budget runs out, the one
 * that straddles the edge is clipped by grapheme cluster (never mid-cluster,
 * never half a wide glyph), and the fill is appended as an **unstyled** span.
 *
 * The fill is unstyled on purpose. Extending the last span's style would drag a
 * selection background or an error colour across the rest of the row, so a
 * one-word red status would paint eighty columns red — the "Christmas tree"
 * §3.10 names. A caller that genuinely wants a full-width band says so by
 * passing a background to {@link fillRow}.
 */
export function padRow(row: StyledRow, cols: number): StyledRow {
  if (cols <= 0) return [];
  const out: Span[] = [];
  let used = 0;
  for (const span of row) {
    if (used >= cols) break;
    const width = displayWidth(span.text);
    if (used + width <= cols) {
      if (span.text.length > 0) out.push(span);
      used += width;
      continue;
    }
    const head = clip(span.text, cols - used);
    if (head.length > 0) {
      out.push({ ...span, text: head });
      used += displayWidth(head);
    }
    break;
  }
  if (used < cols) out.push(plain(" ".repeat(cols - used)));
  return out;
}

/**
 * Pad a styled row to `cols`, drawing the padding in `style` too.
 *
 * The full-width variant of {@link padRow}, for the one case that wants it: a
 * selected row whose background must reach the right edge, because a highlight
 * that stops at the end of the text reads as a highlighted *word* rather than a
 * selected *row*.
 */
export function fillRow(
  row: StyledRow,
  cols: number,
  style: SpanStyle,
): StyledRow {
  const padded = padRow(row, cols);
  return padded.map((span) =>
    span.text.trim().length === 0 && span.fg === undefined
      ? { ...style, text: span.text }
      : span,
  );
}

/**
 * Restyle every span in a row that carries no explicit foreground.
 *
 * The mechanism behind "a whole region recedes": a drawer's inactive rows, a
 * demoted list row. Spans that already named a token keep it, so a status
 * suffix stays meaningful inside a receded row rather than being flattened with
 * everything else — which is the difference between recession and erasure.
 */
export function recede(row: StyledRow, fg: TokenName): StyledRow {
  return row.map((span) => (span.fg === undefined ? { ...span, fg } : span));
}

/**
 * Split a styled row at a column offset, keeping styles intact.
 *
 * Used where a renderer builds a row as text and needs to attribute a leading
 * marker or a trailing suffix without rebuilding the layout — the split is by
 * display column, so a wide glyph is never cut in half.
 */
export function splitAt(
  row: StyledRow,
  cols: number,
): [StyledRow, StyledRow] {
  const head: Span[] = [];
  const tail: Span[] = [];
  let used = 0;
  for (const span of row) {
    if (used >= cols) {
      tail.push(span);
      continue;
    }
    const width = displayWidth(span.text);
    if (used + width <= cols) {
      head.push(span);
      used += width;
      continue;
    }
    // Straddles the boundary: cut by cluster so a wide glyph stays whole.
    let headText = "";
    let tailText = "";
    let taken = used;
    for (const cluster of graphemes(span.text)) {
      const w = clusterWidth(cluster);
      if (taken + w <= cols) {
        headText += cluster;
        taken += w;
      } else {
        tailText += cluster;
      }
    }
    if (headText.length > 0) head.push({ ...span, text: headText });
    if (tailText.length > 0) tail.push({ ...span, text: tailText });
    used = cols;
  }
  return [head, tail];
}
