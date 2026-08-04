/**
 * `tui-check-mocks` — every fenced mock's measured width must match its label.
 *
 * Implements the gate DESIGN.md §3.1 specifies and §6.2(a) wires into CI:
 *
 * > Every mock in this document previously carried a declared tier size that it
 * > was not drawn at — this one said "XL, 160×44" while measuring 118×31; §3.2
 * > said 120×36 and measured 104×22 [...] **No tier boundary in §3.9 had
 * > therefore ever been validated against real content**, which is how the MD
 * > floor ended up where it did. [...] `just tui-check-mocks` parses every
 * > fenced block in this file, computes its display width [...] and **fails if
 * > the measured width does not match the label**.
 *
 * Usage: `bun run scripts/check-mocks.ts <path-to-DESIGN.md>`
 *
 * # Why the width function is implemented here rather than imported
 *
 * §3.1 asks for "the same grapheme/east-asian-width function the renderer
 * uses". OpenTUI does not export one: its width logic lives in the Zig core
 * (`grapheme.zig` / `utf8.zig`) behind the FFI, and `@opentui/core`'s public
 * surface has no string-width entry point (only an ASCII-font `measureText`).
 * So this is an **independent implementation of the same rules** — wcwidth
 * semantics over grapheme clusters — and that difference is load-bearing to
 * state: if the renderer and this checker ever disagree, the doc can pass while
 * the app misaligns.
 *
 * The honest resolution is to export the width function from the renderer and
 * import it here once the T1 harness (§5.3) needs it too; that is tracked as
 * the follow-up below rather than papered over.
 *
 * TODO(wave1, §5.3): when the headless render harness lands, replace
 * {@link displayWidth} with the renderer's own exported measurement so the doc
 * gate and the snapshot suite cannot disagree about what 118 columns means.
 * §3.9's ambiguous-width policy applies then too: nearly all of the doc's
 * chrome (`● ○ ◐ ★ ♥ ▏ ─ │ ┌ └`) is East-Asian **Ambiguous** — width 1 under a
 * Latin locale, width 2 under a CJK-ambiguous setting — so this checker
 * measures the default (Latin) policy, matching the labels as drawn.
 */

/**
 * East-Asian Wide and Fullwidth ranges (UAX #11), the characters that occupy
 * two terminal columns under **every** width policy.
 *
 * Ambiguous-width characters are deliberately **not** here: they are width 1
 * under the default Latin policy the doc's mocks are drawn against. See the
 * module TODO.
 */
const WIDE_RANGES: ReadonlyArray<readonly [number, number]> = [
  [0x1100, 0x115f], // Hangul Jamo init. consonants
  [0x2e80, 0x303e], // CJK Radicals, Kangxi, CJK Symbols
  [0x3041, 0x33ff], // Hiragana, Katakana, Bopomofo, CJK Compatibility
  [0x3400, 0x4dbf], // CJK Unified Ideographs Extension A
  [0x4e00, 0x9fff], // CJK Unified Ideographs
  [0xa000, 0xa4cf], // Yi Syllables
  [0xac00, 0xd7a3], // Hangul Syllables
  [0xf900, 0xfaff], // CJK Compatibility Ideographs
  [0xfe10, 0xfe19], // Vertical forms
  [0xfe30, 0xfe6f], // CJK Compatibility Forms
  [0xff00, 0xff60], // Fullwidth Forms
  [0xffe0, 0xffe6], // Fullwidth signs
  [0x1f300, 0x1f64f], // Misc Symbols and Pictographs, Emoticons
  [0x1f900, 0x1f9ff], // Supplemental Symbols and Pictographs
  [0x20000, 0x2fffd], // CJK Extension B+
  [0x30000, 0x3fffd],
];

/** Zero-width: combining marks, variation selectors, ZWJ. */
const ZERO_WIDTH_RANGES: ReadonlyArray<readonly [number, number]> = [
  [0x0300, 0x036f], // Combining Diacritical Marks
  [0x200b, 0x200f], // ZWSP..RLM (includes ZWJ at 0x200d)
  [0xfe00, 0xfe0f], // Variation Selectors
  [0xfe20, 0xfe2f], // Combining Half Marks
  [0xe0100, 0xe01ef], // Variation Selectors Supplement
];

function inRanges(
  cp: number,
  ranges: ReadonlyArray<readonly [number, number]>,
): boolean {
  for (const [lo, hi] of ranges) {
    if (cp >= lo && cp <= hi) return true;
  }
  return false;
}

/** Columns occupied by one code point. */
function codePointWidth(cp: number): number {
  if (cp === 0) return 0;
  if (inRanges(cp, ZERO_WIDTH_RANGES)) return 0;
  if (inRanges(cp, WIDE_RANGES)) return 2;
  return 1;
}

/**
 * Display width of a line in terminal columns.
 *
 * Measured over **grapheme clusters**, not code points, so a ZWJ emoji sequence
 * or a combining-mark cluster counts once. `Intl.Segmenter` is the standard
 * grapheme segmenter and is available in Bun.
 */
export function displayWidth(line: string): number {
  const segmenter = new Intl.Segmenter("en", { granularity: "grapheme" });
  let width = 0;
  for (const { segment } of segmenter.segment(line)) {
    // A cluster's width is its widest constituent: a base character plus
    // combining marks is as wide as the base.
    let clusterWidth = 0;
    for (const ch of segment) {
      clusterWidth = Math.max(
        clusterWidth,
        codePointWidth(ch.codePointAt(0) ?? 0),
      );
    }
    width += clusterWidth;
  }
  return width;
}

/** A fenced block with the size label that precedes it. */
interface LabelledMock {
  /** 1-based line number of the opening fence. */
  line: number;
  /** Declared columns from the label. */
  declaredCols: number;
  /** Declared rows from the label. */
  declaredRows: number;
  /** Measured columns — the widest line in the block. */
  measuredCols: number;
  /** Measured rows — the number of lines in the block. */
  measuredRows: number;
  /** The label text the numbers came from, for the failure message. */
  label: string;
}

/**
 * Extract every fenced block that carries a `<cols>×<rows>` size label.
 *
 * The label is searched for in the prose preceding the fence, which is how
 * §3.1, §3.2, §3.4, §3.4.1, and §3.9 write them: "drawn at 104×22", "Drawn at
 * 104×13", "XS, drawn at 50×24".
 *
 * A fenced block with **no** label is not an error — §2.1's process diagram and
 * §5.x's shell snippets are not mocks and have no tier to validate against.
 * Only a label that disagrees with its block is a failure.
 *
 * The lookback is generous because §3.1's own label sits **15 lines** above its
 * fence, separated by the blockquote that explains this very gate. A tighter
 * window silently skips the largest mock in the document — the one the doc
 * names as historically mislabelled ("XL, 160×44" while measuring 118×31) —
 * which would be a gate that passes by not looking. The nearest *preceding*
 * label wins, so a generous window cannot steal a label from an earlier block:
 * the scan stops at the previous fence.
 */
const LABEL_LOOKBACK = 24;

export function findLabelledMocks(markdown: string): LabelledMock[] {
  const lines = markdown.split("\n");
  const mocks: LabelledMock[] = [];
  // Where the previous fenced block ended. The lookback never crosses it, so a
  // generous window cannot attribute one block's label to the next.
  let previousFenceEnd = -1;

  for (let i = 0; i < lines.length; i++) {
    const opening = lines[i];
    if (opening === undefined || !opening.startsWith("```")) continue;

    // Find the closing fence.
    let end = i + 1;
    while (end < lines.length && !(lines[end] ?? "").startsWith("```")) end++;
    const body = lines.slice(i + 1, end);

    // Look back for a size label, but never past the previous block. `×` and
    // `x` are both accepted because the doc uses the multiplication sign but a
    // future edit may not.
    const from = Math.max(0, i - LABEL_LOOKBACK, previousFenceEnd + 1);
    const context = lines.slice(from, i).join(" ");
    const match = context.match(/(?:drawn at|at)\s+(\d+)\s*[×x]\s*(\d+)/i);

    if (match?.[1] && match[2] && body.length > 0) {
      mocks.push({
        line: i + 1,
        declaredCols: Number(match[1]),
        declaredRows: Number(match[2]),
        measuredCols: Math.max(...body.map(displayWidth)),
        measuredRows: body.length,
        label: match[0],
      });
    }

    previousFenceEnd = end;
    i = end;
  }

  return mocks;
}

async function main(): Promise<void> {
  const path = process.argv[2];
  if (!path) {
    console.error(
      "::error::usage: bun run scripts/check-mocks.ts <path-to-DESIGN.md>",
    );
    process.exit(1);
  }

  const markdown = await Bun.file(path).text();
  const mocks = findLabelledMocks(markdown);

  if (mocks.length === 0) {
    console.error(
      `::error::${path}: no size-labelled mocks found — the parser is broken`,
    );
    process.exit(1);
  }

  const failures: string[] = [];
  for (const mock of mocks) {
    if (mock.measuredCols !== mock.declaredCols) {
      failures.push(
        `${path}:${mock.line}: label says "${mock.label}" ` +
          `but the block measures ${mock.measuredCols} columns`,
      );
    }
    if (mock.measuredRows !== mock.declaredRows) {
      failures.push(
        `${path}:${mock.line}: label says "${mock.label}" ` +
          `but the block measures ${mock.measuredRows} rows`,
      );
    }
  }

  for (const failure of failures) console.error(`::error::${failure}`);

  if (failures.length > 0) {
    console.error(
      `\n${failures.length} mock label(s) drifted from measured reality. ` +
        `A tier boundary validated against a mislabelled mock is not validated at all (§3.1).`,
    );
    process.exit(1);
  }

  console.log(
    `tui-check-mocks: ${mocks.length} labelled mocks match their declared size`,
  );
  for (const mock of mocks) {
    console.log(
      `  ${path}:${mock.line}  ${mock.measuredCols}x${mock.measuredRows}`,
    );
  }
}

if (import.meta.main) await main();
