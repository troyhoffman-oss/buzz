/**
 * T0 units for the terminal floor — DESIGN.md §3.9 as superseded by
 * NAVIGATION.md §7.
 *
 * This file previously tested a six-tier breakpoint table, a four-region
 * layout selector, and a status-segment drop order. §7 supersedes all three:
 * "**No tier system.** Adaptive reflow per [G10]. Phone is honest terminal
 * width, not a tier." The reflow rules that replaced them are tested in
 * `width.test.ts`, and the "renders identically at every width" property those
 * tiers were a proxy for is now asserted directly by the T1 matrix, which
 * snapshots every screen at 120 **and** 60 columns.
 *
 * What is left is the floor, which §7 did not touch.
 */

import { describe, expect, test } from "bun:test";
import { MIN_COLS, MIN_ROWS, isBelowFloor } from "../../src/shell/tiers";

describe("the supported floor", () => {
  test("is 40x16, not 80x24", () => {
    // §3.9's own history: an earlier draft declared 80x24 while also making the
    // phone a Wave-1 requirement — i.e. declared it both required and
    // unsupported. §1.2 settles it; §7 then removes the tier concept entirely,
    // leaving the floor as the only size constant in the package.
    expect(MIN_COLS).toBe(40);
    expect(MIN_ROWS).toBe(16);
  });

  test("phone-width terminals are above the floor, not below it", () => {
    expect(isBelowFloor(50, 20)).toBe(false);
    expect(isBelowFloor(40, 16)).toBe(false);
    expect(isBelowFloor(60, 24)).toBe(false);
  });

  test("either dimension can trip it", () => {
    expect(isBelowFloor(39, 24)).toBe(true);
    expect(isBelowFloor(80, 15)).toBe(true);
  });
});

describe("§7 removed the tier system", () => {
  test("no breakpoint table, layout selector, or tier function is exported", async () => {
    // A superseded mechanism that still compiles is one the next author will
    // wire back in, and a single `tierFor(cols)` call inside a screen would
    // reintroduce exactly the side-by-side layout §1 says does not survive the
    // port. This asserts the deletion rather than trusting it.
    const module = await import("../../src/shell/tiers");
    for (const removed of [
      "tierFor",
      "layoutFor",
      "TIERS",
      "COLLAPSED_LIST_COLS",
      "STATUS_SEGMENTS",
      "visibleSegments",
      "UNDROPPABLE_SEGMENTS",
    ]) {
      expect(Object.hasOwn(module, removed)).toBe(false);
    }
  });
});
