/**
 * Terminal display width and the truncation rules — NAVIGATION.md §1.2, §2.1,
 * §3, and DESIGN.md §3.9 as superseded by NAVIGATION.md §7.
 *
 * There is **no tier system here**. §7 replaces DESIGN §3.9's breakpoints with
 * adaptive reflow: the same content renders at every width, and only *how* it
 * is cut changes. That makes width a parameter of a pure function rather than a
 * branch over named sizes, which is why every renderer in this package takes
 * `cols` and returns rows rather than consulting a tier table.
 *
 * Four cutting rules, each from a different clause of §7's reflow paragraph and
 * each with a different failure if you get it backwards:
 *
 * - **List rows truncate keeping status** ({@link truncateKeepingSuffix}). The
 *   label ellipsizes; the `⤷ 4 · 2 new` counts do not. §3 makes this explicit —
 *   picking a thread in a busy channel is *the* narrow-width task, and it is
 *   the counts you are picking by.
 * - **The breadcrumb elides from the left** ({@link elideFromLeft}), never the
 *   right: the current location survives, the path is what is sacrificed [G10].
 * - **Statusline rows truncate and never wrap** ({@link truncate}) — a
 *   fixed 3-row band that grows is a band that evicts chat.
 * - **Key hints wrap and never truncate** ({@link wrapHints}) — exits stay
 *   discoverable at every width [G14].
 */

/**
 * East-Asian Wide and Fullwidth ranges (UAX #11): two columns under every
 * width policy.
 *
 * Ambiguous-width characters (`● ○ ◐ ★ ♥ ▏ ─ │ ⤷ ❯ ▌`) are deliberately absent
 * — they are width 1 under the default Latin policy, which is the policy the
 * design's mocks are drawn against and the one `scripts/check-mocks.ts`
 * measures. Keeping the two tables identical is what stops the doc gate and the
 * renderer disagreeing about what 120 columns means.
 */
const WIDE_RANGES: ReadonlyArray<readonly [number, number]> = [
  [0x1100, 0x115f],
  [0x2e80, 0x303e],
  [0x3041, 0x33ff],
  [0x3400, 0x4dbf],
  [0x4e00, 0x9fff],
  [0xa000, 0xa4cf],
  [0xac00, 0xd7a3],
  [0xf900, 0xfaff],
  [0xfe10, 0xfe19],
  [0xfe30, 0xfe6f],
  [0xff00, 0xff60],
  [0xffe0, 0xffe6],
  [0x1f300, 0x1f64f],
  [0x1f900, 0x1f9ff],
  [0x20000, 0x2fffd],
  [0x30000, 0x3fffd],
];

/** Zero-width: combining marks, variation selectors, ZWJ. */
const ZERO_WIDTH_RANGES: ReadonlyArray<readonly [number, number]> = [
  [0x0300, 0x036f],
  [0x200b, 0x200f],
  [0xfe00, 0xfe0f],
  [0xfe20, 0xfe2f],
  [0xe0100, 0xe01ef],
];

function inRanges(
  cp: number,
  ranges: ReadonlyArray<readonly [number, number]>,
): boolean {
  for (const [lo, hi] of ranges) if (cp >= lo && cp <= hi) return true;
  return false;
}

function codePointWidth(cp: number): number {
  if (cp === 0) return 0;
  if (inRanges(cp, ZERO_WIDTH_RANGES)) return 0;
  if (inRanges(cp, WIDE_RANGES)) return 2;
  return 1;
}

/**
 * Grapheme segmenter, constructed once.
 *
 * `new Intl.Segmenter(...)` per call is measurably the hot path when every row
 * of a 400-message timeline is measured on every render, and the object is
 * stateless.
 */
const GRAPHEMES = new Intl.Segmenter("en", { granularity: "grapheme" });

/** Split a string into grapheme clusters. */
export function graphemes(text: string): string[] {
  const out: string[] = [];
  for (const { segment } of GRAPHEMES.segment(text)) out.push(segment);
  return out;
}

/** Display width of one grapheme cluster: the width of its widest code point. */
export function clusterWidth(cluster: string): number {
  let width = 0;
  for (const ch of cluster)
    width = Math.max(width, codePointWidth(ch.codePointAt(0) ?? 0));
  return width;
}

/** Display width of a string in terminal columns, over grapheme clusters. */
export function displayWidth(text: string): number {
  let width = 0;
  for (const cluster of graphemes(text)) width += clusterWidth(cluster);
  return width;
}

/**
 * Cut a string to at most `cols` columns, from the right.
 *
 * Never splits a grapheme cluster and never emits a partial wide character —
 * a half-drawn `💯` is one column of garbage that shifts every column after it
 * on the row.
 */
export function clip(text: string, cols: number): string {
  if (cols <= 0) return "";
  let out = "";
  let used = 0;
  for (const cluster of graphemes(text)) {
    const w = clusterWidth(cluster);
    if (used + w > cols) break;
    out += cluster;
    used += w;
  }
  return out;
}

/** Ellipsis used by every truncation in this package. One column. */
export const ELLIPSIS = "…";

/**
 * Truncate to `cols`, marking the cut with an ellipsis.
 *
 * The result is always ≤ `cols` columns. At `cols` 1 the ellipsis alone is the
 * honest answer; at 0 the honest answer is nothing.
 */
export function truncate(text: string, cols: number): string {
  if (cols <= 0) return "";
  if (displayWidth(text) <= cols) return text;
  if (cols === 1) return ELLIPSIS;
  return `${clip(text, cols - 1)}${ELLIPSIS}`;
}

/**
 * Truncate `label` so that `label + suffix` fits `cols`, **keeping the whole
 * suffix**, with the suffix right-aligned — NAVIGATION.md §3, the list-row rule
 * under [G10].
 *
 * > Reply counts and unread-reply counts render as a status suffix that
 * > survives truncation — the label ellipsizes, the counts do not.
 *
 * Right-aligned because every list row the design draws is:
 *
 * ```
 * ❯ # engineering                                     8 · ⚡claude-1 goose-1
 *   matt · #engineering  @troy the 44200 cadence is ev…        ⤷ 4 · 2 new
 * ```
 *
 * The counts form a **column**, which is what makes a channel list scannable —
 * left-packing them against a variable-length label puts every count at a
 * different x and turns scanning into reading.
 *
 * When even the suffix cannot fit, the suffix is what survives: the row you are
 * choosing *by* is the last thing to go. A row that keeps the label and drops
 * `⤷ 11 · 1 new` is a row you cannot pick a thread from, which is the one
 * narrow-width task §3 names.
 */
export function truncateKeepingSuffix(
  label: string,
  suffix: string,
  cols: number,
  gap = 1,
): string {
  if (cols <= 0) return "";
  if (suffix.length === 0) return truncate(label, cols);
  const suffixWidth = displayWidth(suffix);
  if (suffixWidth >= cols) return clip(suffix, cols);
  const room = cols - suffixWidth - gap;
  if (room <= 0) return clip(suffix, cols);
  const shown = truncate(label, room);
  const fill = cols - displayWidth(shown) - suffixWidth;
  return `${shown}${" ".repeat(Math.max(gap, fill))}${suffix}`;
}

/**
 * Elide from the **left**, keeping the tail — NAVIGATION.md §1.2.
 *
 * > It elides from the left under narrowing (`… › #engineering › ⤷ read-state…`),
 * > never from the right: the current location survives, the path is what is
 * > sacrificed [G10].
 *
 * The inverse of {@link truncate}, and applying the wrong one to the breadcrumb
 * produces `home › channels › #engi…` — a breadcrumb that tells you where you
 * started and not where you are, which is the opposite of its job at depth 5.
 */
export function elideFromLeft(text: string, cols: number): string {
  if (cols <= 0) return "";
  if (displayWidth(text) <= cols) return text;
  if (cols === 1) return ELLIPSIS;
  const clusters = graphemes(text);
  let used = 0;
  const tail: string[] = [];
  for (let i = clusters.length - 1; i >= 0; i--) {
    const cluster = clusters[i] ?? "";
    const w = clusterWidth(cluster);
    if (used + w > cols - 1) break;
    tail.unshift(cluster);
    used += w;
  }
  return `${ELLIPSIS}${tail.join("")}`;
}

/**
 * Pad a string to exactly `cols` columns, clipping if it overflows.
 *
 * Every row this package emits is exactly the terminal width, so a row can
 * never inherit the tail of the row a previous frame drew there.
 */
export function pad(text: string, cols: number): string {
  const clipped = clip(text, cols);
  return clipped + " ".repeat(Math.max(0, cols - displayWidth(clipped)));
}

/**
 * Lay a right-aligned segment into a left-aligned one on a single row.
 *
 * The left side truncates; the right side is placed whole. Used by every
 * `label … 14:09` row in the design, where the timestamp is the fixed part.
 */
export function alignRight(left: string, right: string, cols: number): string {
  if (cols <= 0) return "";
  const rightWidth = displayWidth(right);
  if (rightWidth >= cols) return pad(clip(right, cols), cols);
  const leftText = truncate(left, cols - rightWidth - 1);
  const gap = cols - displayWidth(leftText) - rightWidth;
  return `${leftText}${" ".repeat(Math.max(0, gap))}${right}`;
}

/**
 * Wrap body text to `cols`, breaking on spaces, with an optional hanging
 * indent for continuation rows — §7's "detail fields wrap with hanging indent".
 *
 * A word longer than the available width is hard-split rather than allowed to
 * overflow: an unbroken 200-character URL must not push the frame wider than
 * the terminal.
 */
export function wrapText(
  text: string,
  cols: number,
  hangingIndent = 0,
): string[] {
  if (cols <= 0) return [];
  const rows: string[] = [];
  const words = text.split(/\s+/).filter((w) => w.length > 0);
  if (words.length === 0) return [""];
  const indent = " ".repeat(Math.min(hangingIndent, Math.max(0, cols - 1)));
  let current = "";
  let limit = cols;

  const push = (): void => {
    rows.push(current);
    current = indent;
    limit = cols;
  };

  /** Width available on a *fresh* row — what decides if a word can ever fit. */
  const freshRoom = cols - displayWidth(indent);

  for (const word of words) {
    let remaining = word;
    while (
      displayWidth(remaining) >
      limit - displayWidth(current) - (current.trim() ? 1 : 0)
    ) {
      const room = limit - displayWidth(current) - (current.trim() ? 1 : 0);
      // **Only hard-split a word that cannot fit on a fresh row.** Splitting
      // whenever the *current* row is short is what produced `passp` / `hrase`
      // in the 60-column onboarding capture: "passphrase" is 10 characters and
      // the body is 58 wide, so it fits perfectly well one line down. Breaking
      // a word that had somewhere to go is not reflow, it is corruption — §7's
      // rule is that narrowing changes how content is *cut*, and a word cut
      // through its middle is unreadable at exactly the width where reading is
      // already hardest.
      if (
        displayWidth(remaining) > freshRoom &&
        room >= Math.min(4, freshRoom)
      ) {
        // Genuinely unbreakable — a 200-character URL. Place what fits and
        // carry the rest, because the alternative is overflowing the frame.
        const head = clip(remaining, room);
        current = current.trim() ? `${current} ${head}` : `${current}${head}`;
        remaining = remaining.slice(head.length);
        push();
      } else if (current.trim() || displayWidth(indent) > 0) {
        // It fits on a fresh row: start one rather than breaking the word.
        push();
        // A degenerate hanging indent could leave a row that still cannot hold
        // the word. Without this the loop would `push()` forever on an
        // indent-only row, hanging the render rather than misdrawing it.
        if (displayWidth(remaining) > freshRoom) continue;
        break;
      } else {
        // A fresh, unindented row still cannot hold it: split at full width.
        const head = clip(remaining, limit - displayWidth(current));
        if (head.length === 0) return rows;
        current = `${current}${head}`;
        remaining = remaining.slice(head.length);
        push();
      }
    }
    current = current.trim()
      ? `${current} ${remaining}`
      : `${current}${remaining}`;
  }
  if (current.length > 0) rows.push(current);
  return rows;
}

/**
 * Lay key hints across as many rows as they need — NAVIGATION.md §2.3 [G14].
 *
 * > the key-hint footer **wraps rather than truncates** — exits stay
 * > discoverable at every width.
 *
 * This is the one place in the design where growing the band is correct: a
 * truncated `↑/↓ select · ⏎ expand · → jump in · esc c…` hides the escape hatch
 * at exactly the width where the user is most likely to need it. Hints are
 * never split mid-hint; a single hint wider than the terminal is clipped, since
 * there is no honest alternative.
 */
export function wrapHints(hints: readonly string[], cols: number): string[] {
  if (cols <= 0 || hints.length === 0) return [];
  const separator = " · ";
  const rows: string[] = [];
  let current = "";
  for (const hint of hints) {
    const candidate =
      current.length === 0 ? hint : `${current}${separator}${hint}`;
    if (displayWidth(candidate) <= cols) {
      current = candidate;
      continue;
    }
    if (current.length > 0) rows.push(current);
    current = displayWidth(hint) <= cols ? hint : clip(hint, cols);
  }
  if (current.length > 0) rows.push(current);
  return rows;
}
