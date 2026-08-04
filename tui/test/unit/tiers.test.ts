/**
 * T0 unit tests for the responsive tier table — DESIGN.md §3.9, §5.2.
 *
 * Pure functions over data, tested with no terminal at all (§5.2).
 */

import { describe, expect, test } from "bun:test";
import {
  COLLAPSED_LIST_COLS,
  MIN_COLS,
  MIN_ROWS,
  STATUS_SEGMENTS,
  type StatusSegment,
  UNDROPPABLE_SEGMENTS,
  isBelowFloor,
  layoutFor,
  tierFor,
  visibleSegments,
} from "../../src/shell/tiers";

describe("tier resolution", () => {
  test("matches the §3.9 breakpoint table", () => {
    expect(tierFor(180)).toBe("xl");
    expect(tierFor(160)).toBe("xl");
    expect(tierFor(159)).toBe("lg");
    expect(tierFor(120)).toBe("lg");
    expect(tierFor(119)).toBe("md");
    expect(tierFor(90)).toBe("md");
    expect(tierFor(89)).toBe("mdn");
    expect(tierFor(72)).toBe("mdn");
    expect(tierFor(71)).toBe("sm");
    expect(tierFor(60)).toBe("sm");
    expect(tierFor(59)).toBe("xs");
    expect(tierFor(40)).toBe("xs");
  });

  test("the §5.3 snapshot matrix widths land on their named tiers", () => {
    // snapshots/<screen>/<tier>.txt — xs(50x20) sm(70x24) mdn(80x28)
    // md(100x30) lg(140x40) xl(180x50)
    expect(tierFor(50)).toBe("xs");
    expect(tierFor(70)).toBe("sm");
    expect(tierFor(80)).toBe("mdn");
    expect(tierFor(100)).toBe("md");
    expect(tierFor(140)).toBe("lg");
    expect(tierFor(180)).toBe("xl");
  });

  test("MD-narrow keeps channel context, losing only names", () => {
    // §3.9: "Losing channel *names* is much cheaper than losing channel
    // *context*, so the collapsed 18-column list keeps glyph + unread count."
    const mdn = layoutFor(72);
    expect(mdn.list).toBe("collapsed");
    expect(COLLAPSED_LIST_COLS).toBe(18);

    // The band this replaced: MD ≥90 → SM ≥60 would have dropped 72 to
    // "main only; list is a dialog".
    expect(layoutFor(60).list).toBe("dialog");
  });

  test("only XL draws the community rail", () => {
    expect(layoutFor(160).rail).toBe(true);
    expect(layoutFor(120).rail).toBe(false);
  });

  test("aux is a pane at LG and up, an overlay through MD-narrow, gone at XS", () => {
    expect(layoutFor(160).aux).toBe("pane");
    expect(layoutFor(120).aux).toBe("pane");
    expect(layoutFor(90).aux).toBe("overlay");
    expect(layoutFor(72).aux).toBe("overlay");
    expect(layoutFor(50).aux).toBe("none");
  });
});

describe("minimum size", () => {
  test("the floor is 40x16, not 80x24", () => {
    // §3.9: an earlier draft declared 80x24 while also making SM/XS Wave-1
    // requirements — declaring the phone tier both required and unsupported.
    expect(MIN_COLS).toBe(40);
    expect(MIN_ROWS).toBe(16);
  });

  test("XS sizes are above the floor, not below it", () => {
    expect(isBelowFloor(50, 20)).toBe(false);
    expect(isBelowFloor(40, 16)).toBe(false);
  });

  test("either dimension can trip the floor", () => {
    expect(isBelowFloor(39, 24)).toBe(true);
    expect(isBelowFloor(80, 15)).toBe(true);
  });
});

describe("status bar segment priority", () => {
  const uniform = Object.fromEntries(
    STATUS_SEGMENTS.map((s) => [s, 10]),
  ) as Record<StatusSegment, number>;

  test("connection and pending-leader survive every tier", () => {
    // §3.9: "the first two never drop". At a width that fits nothing, they are
    // still returned — a truncated connection indicator would fail §1.3
    // property 3 exactly where it matters most.
    expect(visibleSegments(0, uniform)).toEqual([...UNDROPPABLE_SEGMENTS]);
    expect(visibleSegments(5, uniform)).toEqual([...UNDROPPABLE_SEGMENTS]);
  });

  test("segments drop right-to-left in the declared order", () => {
    expect(visibleSegments(30, uniform)).toEqual([
      "connection",
      "pending-leader",
      "mentions",
    ]);
    expect(visibleSegments(40, uniform)).toEqual([
      "connection",
      "pending-leader",
      "mentions",
      "unread",
    ]);
  });

  test("help is the first to go and connection the last", () => {
    expect(STATUS_SEGMENTS[0]).toBe("connection");
    expect(STATUS_SEGMENTS[STATUS_SEGMENTS.length - 1]).toBe("help");
  });

  test("a wide bar carries every segment", () => {
    expect(visibleSegments(1000, uniform)).toEqual([...STATUS_SEGMENTS]);
  });
});
