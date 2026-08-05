/**
 * T0 units for the reflow primitives — NAVIGATION.md §7, §1.2, §3, §2.3.
 *
 * These are the functions §7 substitutes for DESIGN §3.9's tier table, so the
 * cases below are the *rules* rather than the arithmetic: which end a string
 * loses, what survives a cut, and what is allowed to grow.
 */

import { describe, expect, test } from "bun:test";
import {
  clip,
  displayWidth,
  elideFromLeft,
  graphemes,
  pad,
  truncate,
  truncateKeepingSuffix,
  wrapHints,
  wrapText,
} from "../../src/render/width";

describe("displayWidth", () => {
  test("ASCII is one column per character", () => {
    expect(displayWidth("hello")).toBe(5);
    expect(displayWidth("")).toBe(0);
  });

  test("the design's chrome is single-width under the Latin policy", () => {
    // Every one of these is East-Asian Ambiguous. The doc's mocks are drawn
    // against the Latin policy, and `scripts/check-mocks.ts` measures the same
    // way — if the two tables ever disagree, the doc passes while the app
    // misaligns.
    expect(displayWidth("❯ ▌ ⤷ ● ○ ◐ ◌ ★ ♥")).toBe(17);
    expect(displayWidth("╭──╮")).toBe(4);
  });

  test("CJK and wide emoji occupy two columns", () => {
    expect(displayWidth("日本語")).toBe(6);
    expect(displayWidth("💯")).toBe(2);
  });

  test("⚡ is Wide, not Ambiguous — the agent-working marker", () => {
    // U+26A1 is `eaw=W` in UAX #11, which is not a policy choice the way the
    // Ambiguous set above is: every terminal advances two cells for it.
    //
    // It was missing from the table for three milestones, and the cost was not
    // a rounding error. `⚡` rides the channel-list and fleet status suffixes —
    // exactly the rows §3's list-row rule works hardest to keep intact — so a
    // row measured one column short overflowed the pane, the terminal wrapped
    // it, and `⚡claude-1 goose-1` rendered as `⚡claude-1 goose-` with a bare
    // `1` alone on the next line. Visible in the M1, M2 and M3 captures at both
    // widths, and read as a layout bug rather than a measurement one.
    expect(displayWidth("⚡")).toBe(2);
    expect(displayWidth("⚡claude-1")).toBe(10);
  });

  test("a ZWJ sequence counts once, not once per code point", () => {
    expect(displayWidth("\u{1F468}‍\u{1F4BB}")).toBe(2);
    expect(graphemes("\u{1F468}‍\u{1F4BB}")).toHaveLength(1);
  });
});

describe("clip never splits a wide cluster", () => {
  test("a wide character is dropped rather than half-drawn", () => {
    // Half a `💯` is one column of garbage that shifts every column after it on
    // the row, so the honest answer at an odd boundary is to stop early.
    expect(clip("a💯b", 2)).toBe("a");
    expect(displayWidth(clip("a💯b", 2))).toBeLessThanOrEqual(2);
  });

  test("the result never exceeds the requested width", () => {
    for (const cols of [0, 1, 2, 3, 4, 5]) {
      expect(displayWidth(clip("日本語abc", cols))).toBeLessThanOrEqual(cols);
    }
  });
});

describe("truncate — statusline rows and labels (§2.1, §7)", () => {
  test("a fitting string is returned untouched", () => {
    expect(truncate("#engineering", 20)).toBe("#engineering");
  });

  test("an overlong string is cut from the right with an ellipsis", () => {
    expect(truncate("#engineering", 6)).toBe("#engi…");
    expect(displayWidth(truncate("#engineering", 6))).toBe(6);
  });

  test("at one column the ellipsis alone is the honest answer", () => {
    expect(truncate("#engineering", 1)).toBe("…");
  });
});

describe("truncateKeepingSuffix — the list-row rule (§3, [G10])", () => {
  test("the label ellipsizes and the counts do not", () => {
    // §3: "Reply counts and unread-reply counts render as a status suffix that
    // survives truncation." Picking a thread in a busy channel is the narrow-
    // width task, and it is the counts you are picking by.
    const row = truncateKeepingSuffix(
      "read-state slots cap at 8 — the 9th write is the interesting one",
      "⤷ 11 · 1 new",
      40,
    );
    expect(row).toContain("⤷ 11 · 1 new");
    expect(row).toContain("…");
    expect(displayWidth(row)).toBeLessThanOrEqual(40);
  });

  test("when only one can fit, the suffix is what survives", () => {
    // A row that keeps the label and drops the counts is a row you cannot pick
    // a thread from.
    const row = truncateKeepingSuffix(
      "a very long thread title here",
      "⤷ 4 · 2 new",
      12,
    );
    expect(row).toContain("⤷ 4");
    expect(displayWidth(row)).toBeLessThanOrEqual(12);
  });

  test("a fitting pair keeps both whole", () => {
    expect(truncateKeepingSuffix("engineering", "⤷ 4", 40)).toContain(
      "engineering",
    );
    expect(truncateKeepingSuffix("engineering", "⤷ 4", 40)).toContain("⤷ 4");
  });
});

describe("elideFromLeft — the breadcrumb rule (§1.2, [G10])", () => {
  test("the current location survives; the path is sacrificed", () => {
    const crumb = "home › channels › #engineering › ⤷ read-state slots";
    const elided = elideFromLeft(crumb, 30);
    expect(elided.startsWith("…")).toBe(true);
    expect(elided).toContain("read-state slots");
    expect(elided).not.toContain("home");
    expect(displayWidth(elided)).toBeLessThanOrEqual(30);
  });

  test("it is the inverse of truncate, which would keep the wrong end", () => {
    // Applying `truncate` here yields `home › channels › #engi…` — a breadcrumb
    // that tells you where you started rather than where you are, which is the
    // opposite of its job at depth 5.
    const crumb = "home › channels › #engineering";
    expect(elideFromLeft(crumb, 16)).not.toEqual(truncate(crumb, 16));
    expect(elideFromLeft(crumb, 16)).toContain("engineering");
  });

  test("a fitting crumb is returned whole", () => {
    expect(elideFromLeft("home", 20)).toBe("home");
  });
});

describe("wrapHints — exits stay discoverable (§2.3, [G14])", () => {
  test("hints wrap rather than truncate", () => {
    const hints = ["↑/↓ select", "⏎ expand", "→ jump in", "esc close"];
    const rows = wrapHints(hints, 24);
    expect(rows.length).toBeGreaterThan(1);
    // The escape hatch must be present in full at every width — a truncated
    // `esc c…` hides it at exactly the width where it is most needed.
    expect(rows.join(" ")).toContain("esc close");
    for (const row of rows) expect(displayWidth(row)).toBeLessThanOrEqual(24);
  });

  test("every hint survives at an absurdly narrow width", () => {
    const hints = ["↑/↓ select", "⏎ expand", "→ jump in", "esc close"];
    const rows = wrapHints(hints, 12);
    for (const hint of hints) expect(rows.join("\n")).toContain(hint);
  });

  test("hints that fit stay on one row", () => {
    expect(wrapHints(["a", "b"], 40)).toEqual(["a · b"]);
  });
});

describe("wrapText — detail fields wrap with hanging indent (§7)", () => {
  test("a wide glyph in one column terminates instead of hanging", () => {
    // **This hung the renderer**, and a hang is worse than any misdraw: it
    // starves the event loop, so the symptom is a frozen terminal with no
    // error and nothing in a log.
    //
    // A two-column cluster in one column of room does not fit at all, so
    // `clip` returned `""`, `remaining` was unchanged, and the loop re-entered
    // on identical state forever. ASCII was fine — `wrapText("abc", 1)` splits
    // per character — which is why it survived: every test used Latin text.
    //
    // Reachable in production through `renderDrawerExpansion`, whose field
    // values wrap into `cols - labelWidth - 2`. A `Channel:` label is 9, so a
    // 12-column terminal with a CJK channel name is one column of room.
    //
    // The assertion is simply *that it returns*. Bun has no per-test timeout by
    // default, so a regression here would hang the suite rather than fail it —
    // which is exactly how this survived three milestones.
    expect(wrapText("日本語", 1, 0)).toEqual([]);
    expect(wrapText("abc", 1, 0)).toEqual(["a", "b", "c"]);
    // The whole degenerate neighbourhood, not just the reported case.
    for (const cols of [1, 2, 3]) {
      for (const text of ["日本語", "a日b", "💯💯", "日 本", "ab日cd"]) {
        const rows = wrapText(text, cols, 0);
        for (const row of rows)
          expect(displayWidth(row)).toBeLessThanOrEqual(cols);
      }
    }
  });

  test("wrapping breaks on spaces", () => {
    const rows = wrapText("the quick brown fox jumps over", 12);
    for (const row of rows) expect(displayWidth(row)).toBeLessThanOrEqual(12);
    expect(rows.join(" ").replace(/\s+/g, " ")).toBe(
      "the quick brown fox jumps over",
    );
  });

  test("an unbreakable token is hard-split rather than overflowing", () => {
    // A 200-character URL must not push the frame wider than the terminal.
    const rows = wrapText("x".repeat(50), 10);
    for (const row of rows) expect(displayWidth(row)).toBeLessThanOrEqual(10);
    expect(rows.join("")).toBe("x".repeat(50));
  });

  test("a word that fits on a fresh row is never split", () => {
    // Found in the 60-column onboarding capture, which rendered `passp` /
    // `hrase`: the old rule split whenever the *current* row was short, so a
    // 10-character word broke in half on a 58-column body it fits on twice
    // over. §7 says narrowing changes how content is cut; a word cut through
    // its middle is not a cut, it is corruption — and it lands at exactly the
    // width where reading is already hardest.
    const rows = wrapText(
      "Write it down somewhere safe. There is no reset: the passphrase",
      58,
    );
    expect(rows).toContain("passphrase");
    for (const row of rows) expect(row).not.toContain("passp\n");
    expect(rows.some((r) => r.endsWith("passp"))).toBe(false);
  });

  test("no word is broken unless it cannot fit on a line of its own", () => {
    // The general property, swept rather than spot-checked: every output
    // fragment must be a whole input word, unless that word is itself wider
    // than the column budget.
    const text = "alpha bb ccc dddd eeeee ffffff ggggggg hhhhhhhh";
    const words = new Set(text.split(" "));
    for (const cols of [10, 12, 16, 20, 24, 40]) {
      for (const row of wrapText(text, cols)) {
        for (const fragment of row.trim().split(/\s+/)) {
          if (fragment.length === 0) continue;
          if (fragment.length >= cols) continue;
          expect(words.has(fragment)).toBe(true);
        }
      }
    }
  });

  test("a hanging indent wider than the content still terminates", () => {
    // The loop's degenerate case: an indent that leaves less room than the
    // word needs could `push()` an indent-only row forever. A hang here would
    // read as the app freezing rather than as a wrap bug.
    const rows = wrapText("alpha betagammadelta", 8, 6);
    expect(rows.length).toBeGreaterThan(1);
    for (const row of rows) expect(displayWidth(row)).toBeLessThanOrEqual(8);
    expect(rows.join("").replace(/\s+/g, "")).toBe("alphabetagammadelta");
  });

  test("continuation rows carry the hanging indent", () => {
    const rows = wrapText("alpha beta gamma delta", 12, 2);
    expect(rows.length).toBeGreaterThan(1);
    expect(rows[1]?.startsWith("  ")).toBe(true);
  });
});

describe("pad", () => {
  test("every row is exactly the terminal width", () => {
    // A short row would let a previous frame's tail survive at that position.
    expect(displayWidth(pad("abc", 10))).toBe(10);
    expect(displayWidth(pad("abcdefghijkl", 10))).toBe(10);
    expect(displayWidth(pad("日本語日本語", 7))).toBeLessThanOrEqual(7);
  });
});
