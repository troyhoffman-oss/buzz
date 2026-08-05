/**
 * The styled-row contract — `src/render/span.ts`.
 *
 * One property carries this whole module, and it is the reason the theme pass
 * could land without forking the render path:
 *
 * > `rowText(padRow(spans, cols))` is byte-identical to `pad(text, cols)`.
 *
 * Every geometry assertion in the T1 matrix, the §4 walkthroughs and the
 * empty-state suite tests the *string* a row renders to. If that projection is
 * total, those suites keep testing exactly what they tested before and the
 * colour is additive. If it is not, they are testing a different renderer than
 * the one the terminal receives, and the matrix stops meaning anything.
 */

import { describe, expect, test } from "bun:test";
import {
  type StyledRow,
  fillRow,
  padRow,
  plain,
  plainRow,
  recede,
  rowText,
  splitAt,
  styled,
} from "../../src/render/span";
import { displayWidth, pad } from "../../src/render/width";

describe("the projection back to string[] is total", () => {
  const cases: ReadonlyArray<readonly [string, StyledRow, number]> = [
    ["plain fits", plainRow("hello"), 20],
    ["plain exact", plainRow("hello"), 5],
    ["plain overflows", plainRow("hello world"), 5],
    [
      "multi-span fits",
      [plain("  "), styled("❯", { fg: "accent" }), plain(" label")],
      20,
    ],
    [
      "multi-span cut mid-span",
      [plain("  "), styled("❯ label", { fg: "accent" }), plain(" tail")],
      6,
    ],
    [
      "wide glyph at the boundary",
      [plain("ab"), styled("日本語", { fg: "primary" })],
      5,
    ],
    ["empty row", [], 8],
    ["zero width", plainRow("hello"), 0],
  ];

  for (const [name, row, cols] of cases) {
    test(name, () => {
      expect(rowText(padRow(row, cols))).toBe(pad(rowText(row), cols));
    });
  }

  test("a padded row is always exactly cols columns", () => {
    for (const cols of [1, 5, 40, 60, 120]) {
      const row = padRow([plain("  "), styled("❯ hi", { fg: "accent" })], cols);
      expect(displayWidth(rowText(row))).toBe(cols);
    }
  });

  test("a wide glyph is never cut in half", () => {
    // A half-drawn wide character is one column of garbage that shifts every
    // column after it on the row — the failure `clip` exists to prevent, and
    // the span path must not reintroduce it at the span boundary.
    const row = padRow([styled("日本語", { fg: "text" })], 3);
    expect(rowText(row)).toBe("日 ");
    expect(displayWidth(rowText(row))).toBe(3);
  });
});

describe("padRow does not bleed style into the padding", () => {
  test("the fill is unstyled", () => {
    // Extending the last span's style would drag a selection background or an
    // error colour across the rest of the row: a one-word red status painting
    // eighty columns red is the "Christmas tree" §3.10 names by that word.
    const row = padRow(
      [styled("err", { fg: "error", bg: "backgroundPanel" })],
      10,
    );
    const fill = row.at(-1);
    expect(fill?.text).toBe("       ");
    expect(fill?.fg).toBeUndefined();
    expect(fill?.bg).toBeUndefined();
  });
});

describe("fillRow is the opt-in full-width band", () => {
  /** Columns actually covered by the fill's background. */
  const covered = (row: StyledRow): number =>
    row
      .filter((span) => span.bg !== undefined)
      .reduce((sum, span) => sum + displayWidth(span.text), 0);

  test("the padding carries the requested style", () => {
    // A selected row whose highlight stops at the end of its text reads as a
    // highlighted *word*, not a selected *row*. This is the one case that
    // wants the fill styled, and it says so explicitly.
    const row = fillRow(plainRow("  row"), 10, { bg: "backgroundElement" });
    expect(rowText(row)).toBe("  row     ");
    expect(row.at(-1)?.bg).toBe("backgroundElement");
  });

  test("the fill covers EVERY column, not only the trailing pad", () => {
    // **The case the two above cannot see.** Both use short content, so they
    // are satisfied by a fill that only styles the padding — and the first
    // implementation did exactly that, restyling spans that were blank *and*
    // had no foreground.
    //
    // Real list rows are not short. `alignRight` and `truncateKeepingSuffix`
    // pad to `cols` themselves, so a selected home or channel row arrives here
    // with **no trailing blank span at all** and the highlight covered zero
    // columns — the owner's "selection is hard to see", reintroduced by the
    // very helper written to fix it. Measuring coverage rather than probing
    // one span is what makes this assertion honest.
    const already = [plain("  "), styled("#engineering        8 · @2", {})];
    for (const cols of [28, 40, 120]) {
      const row = fillRow(already, cols, { bg: "backgroundElement" });
      expect(displayWidth(rowText(row))).toBe(cols);
      expect(covered(row)).toBe(cols);
    }
  });

  test("spans that named their own colour keep it", () => {
    const row = fillRow(
      [plain("  "), styled("8 unread", { fg: "primary" })],
      20,
      { bg: "backgroundElement" },
    );
    // Recession must not be erasure: the count keeps its meaning and merely
    // gains the selected row's background.
    const count = row.find((s) => s.text === "8 unread");
    expect(count?.fg).toBe("primary");
    expect(count?.bg).toBe("backgroundElement");
  });
});

describe("recede restyles only the spans that made no choice", () => {
  test("an explicit token survives", () => {
    // Recession must not be erasure: a status suffix inside a demoted row is
    // still the thing you are scanning by.
    const row = recede(
      [plain("channel"), styled("⤷ 4 · 2 new", { fg: "info" })],
      "textMuted",
    );
    expect(row[0]?.fg).toBe("textMuted");
    expect(row[1]?.fg).toBe("info");
  });
});

describe("splitAt cuts by display column", () => {
  test("splitting inside a span preserves both halves' style", () => {
    const [head, tail] = splitAt([styled("abcdef", { fg: "accent" })], 3);
    expect(rowText(head)).toBe("abc");
    expect(rowText(tail)).toBe("def");
    expect(head[0]?.fg).toBe("accent");
    expect(tail[0]?.fg).toBe("accent");
  });

  test("a wide glyph goes whole to one side", () => {
    const [head, tail] = splitAt([plain("日本語")], 3);
    // `日` is two columns; `本` would straddle 3, so it belongs to the tail.
    expect(rowText(head)).toBe("日");
    expect(rowText(tail)).toBe("本語");
  });

  test("the two halves always reconstruct the whole", () => {
    const row: StyledRow = [
      plain("  "),
      styled("❯ engineering", { fg: "accent" }),
      styled("  8 · 2", { fg: "primary" }),
    ];
    for (let at = 0; at <= displayWidth(rowText(row)) + 2; at++) {
      const [head, tail] = splitAt(row, at);
      expect(rowText(head) + rowText(tail)).toBe(rowText(row));
    }
  });
});
