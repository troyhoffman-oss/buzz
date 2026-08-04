/**
 * Tests for the `tui-check-mocks` gate — DESIGN.md §3.1, §6.2(a).
 *
 * A gate is only worth having if it fails on the thing it exists to catch, so
 * the drift cases below are the point of this file; the passing case is the
 * control.
 */

import { describe, expect, test } from "bun:test";
import { displayWidth, findLabelledMocks } from "../../scripts/check-mocks";

describe("displayWidth", () => {
  test("ASCII is one column per character", () => {
    expect(displayWidth("hello")).toBe(5);
    expect(displayWidth("")).toBe(0);
  });

  test("box-drawing chrome is single-width under the default policy", () => {
    // §3.9: this chrome is East-Asian Ambiguous — width 1 under a Latin locale,
    // which is the policy the doc's mocks are drawn against.
    expect(displayWidth("┌──┐")).toBe(4);
    expect(displayWidth("│ ●9 │")).toBe(6);
    expect(displayWidth("▏⤷ ★ ○ ◐")).toBe(8);
  });

  test("CJK is double-width", () => {
    expect(displayWidth("日本語")).toBe(6);
  });

  test("genuinely wide emoji occupy two columns", () => {
    // §3.9 names `⚡` and `💯` as "genuinely double-width and sit inside
    // fixed-width columns".
    expect(displayWidth("💯")).toBe(2);
  });

  test("a ZWJ sequence counts once, not once per code point", () => {
    // Measured over grapheme clusters. Counting code points would report 4+.
    expect(displayWidth("\u{1F468}‍\u{1F4BB}")).toBe(2);
  });

  test("combining marks do not add width", () => {
    expect(displayWidth("é")).toBe(1);
  });
});

const doc = (label: string, block: string[]): string =>
  ["Some prose.", "", label, "", "```", ...block, "```", ""].join("\n");

describe("findLabelledMocks", () => {
  test("measures a well-formed mock", () => {
    const mocks = findLabelledMocks(doc("Drawn at 4×2:", ["┌──┐", "└──┘"]));
    expect(mocks).toHaveLength(1);
    expect(mocks[0]?.measuredCols).toBe(4);
    expect(mocks[0]?.measuredRows).toBe(2);
    expect(mocks[0]?.declaredCols).toBe(4);
    expect(mocks[0]?.declaredRows).toBe(2);
  });

  test("catches a column label that drifted from the block", () => {
    // §3.1's own history: "this one said 'XL, 160×44' while measuring 118×31".
    const mocks = findLabelledMocks(doc("Drawn at 160×2:", ["┌──┐", "└──┘"]));
    expect(mocks[0]?.declaredCols).toBe(160);
    expect(mocks[0]?.measuredCols).toBe(4);
    expect(mocks[0]?.measuredCols).not.toBe(mocks[0]?.declaredCols);
  });

  test("catches a row label that drifted from the block", () => {
    const mocks = findLabelledMocks(doc("Drawn at 4×44:", ["┌──┐", "└──┘"]));
    expect(mocks[0]?.measuredRows).toBe(2);
    expect(mocks[0]?.declaredRows).toBe(44);
  });

  test("accepts both × and x, and the 'XS, drawn at' phrasing", () => {
    expect(
      findLabelledMocks(doc("XS, drawn at 4x2:", ["┌──┐", "└──┘"])),
    ).toHaveLength(1);
  });

  test("an unlabelled fence is skipped, not failed", () => {
    // §2.1's process diagram and §5.x's shell snippets are not mocks.
    const markdown = ["```bash", "tmux new-session", "```"].join("\n");
    expect(findLabelledMocks(markdown)).toHaveLength(0);
  });

  test("finds a label separated from its fence by a blockquote", () => {
    // §3.1's label sits 15 lines above its fence, across the blockquote that
    // explains this gate. A tight lookback would silently skip the largest
    // mock in the document — a gate that passes by not looking.
    const markdown = [
      "**LG tier, drawn at 4×2.** Marks the focused pane.",
      "",
      "> A blockquote.",
      "> Spanning several lines.",
      "> And several more.",
      "> Still going.",
      "> Nearly done.",
      "> Done.",
      "",
      "```",
      "┌──┐",
      "└──┘",
      "```",
    ].join("\n");
    const mocks = findLabelledMocks(markdown);
    expect(mocks).toHaveLength(1);
    expect(mocks[0]?.declaredCols).toBe(4);
  });

  test("a label is never stolen across a preceding fence", () => {
    // The generous lookback must not attribute one block's label to the next,
    // or an unlabelled block would inherit a size it was never drawn at.
    const markdown = [
      "Drawn at 4×2:",
      "",
      "```",
      "┌──┐",
      "└──┘",
      "```",
      "",
      "Some prose with no size in it at all.",
      "",
      "```",
      "┌────────┐",
      "└────────┘",
      "```",
    ].join("\n");
    const mocks = findLabelledMocks(markdown);
    expect(mocks).toHaveLength(1);
    expect(mocks[0]?.measuredCols).toBe(4);
  });
});
