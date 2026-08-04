/**
 * T1 — the reflow matrix, DESIGN.md §5.3 under NAVIGATION.md §7.
 *
 * §7 replaced the tier system with adaptive reflow, so this suite is **not** a
 * "screen × tier" grid: it snapshots every screen at a **wide** and a **narrow**
 * width and asserts the pair differs only in how content is *cut*, never in
 * what content exists. That is the actual claim §7 makes, and a tier-shaped
 * matrix could not test it — it would only prove that two named layouts each
 * render.
 *
 * The six §5.3 determinism requirements, and how each is met here:
 *
 * 1. **Frozen clock** — `FIXED_NOW`, threaded through `renderScreen`.
 * 2. **Frozen randomness** — nothing samples; ranking is deterministic by
 *    construction (`fuzzyScore`, `rankCandidates` both tiebreak on name).
 * 3. **No animation** — no spinner exists; the drawer's live tail is fixture-fed.
 * 4. **Fixed dimensions** per snapshot, declared in the test name.
 * 5. **Fixed theme, `LANG=C.UTF-8`, `TZ=UTC`** — every clock format in this
 *    package is explicitly UTC (`getUTC*`), never the ambient zone.
 * 6. **Fixture-backed daemon** — `FixtureClient`, never a network.
 *
 * Plus §5.3's **shadow run**: two renders of one input must be byte-identical.
 */

import { describe, expect, test } from "bun:test";
import { Session } from "../helpers/drive";
import { displayWidth } from "../../src/render/width";

/** The two widths every screen is proven at. */
const WIDE = 120;
const NARROW = 60;
const ROWS = 30;

/** Reach each layer by the route a user would take. */
const routes = {
  home: (cols: number) => Session.open("seeded-basic", cols, ROWS),
  channels: (cols: number) =>
    Session.open("seeded-basic", cols, ROWS).goTo("Channels").key("right"),
  channel: (cols: number) =>
    Session.open("seeded-basic", cols, ROWS)
      .goTo("Channels")
      .key("right")
      .key("right"),
  messageSelect: (cols: number) =>
    Session.open("seeded-basic", cols, ROWS)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("up"),
  thread: (cols: number) =>
    Session.open("seeded-basic", cols, ROWS)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("up")
      .key("up", { shift: true })
      .key("right"),
  drawer: (cols: number) =>
    Session.open("seeded-basic", cols, ROWS)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("down"),
  drawerExpanded: (cols: number) =>
    Session.open("seeded-basic", cols, ROWS)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("down")
      .key("down")
      .key("return"),
  activity: (cols: number) =>
    Session.open("seeded-basic", cols, ROWS)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("down")
      .key("down")
      .key("right"),
  keyless: (cols: number) => Session.open("keyless-daemon", cols, ROWS),
  empty: (cols: number) => Session.open("empty", cols, ROWS),
} as const;

type ScreenName = keyof typeof routes;
const SCREENS = Object.keys(routes) as ScreenName[];

describe("every row is exactly the terminal width, at every width", () => {
  for (const name of SCREENS) {
    for (const cols of [WIDE, NARROW]) {
      test(`${name} at ${cols}x${ROWS}`, () => {
        const rows = routes[name](cols).screen();
        expect(rows.length).toBeLessThanOrEqual(ROWS);
        for (const row of rows) {
          // A short row lets a previous frame's tail survive at that position,
          // which on a streaming chat surface reads as a rendering glitch.
          expect(displayWidth(row)).toBe(cols);
        }
      });
    }
  }
});

describe("§5.3 shadow run — two renders of one input are byte-identical", () => {
  for (const name of SCREENS) {
    test(name, () => {
      const a = routes[name](WIDE).text();
      const b = routes[name](WIDE).text();
      expect(a).toBe(b);
    });
  }
});

describe("§7 reflow — the same content at both widths, cut differently", () => {
  test("the timeline keeps every author and every thread count at 60 columns", () => {
    const wide = routes.channel(WIDE).text();
    const narrow = routes.channel(NARROW).text();
    // Authors are content; they survive the cut.
    for (const author of ["ana", "troy", "claude-1", "matt"]) {
      expect(wide).toContain(author);
      expect(narrow).toContain(author);
    }
    // §3's list-row rule: the counts do not truncate. This is the row you pick
    // a thread by, and losing it at 60 columns would make the narrow width a
    // different product rather than a narrower one.
    for (const counts of ["⤷ 11 · 1 new", "⤷ 4 · 2 new", "⤷ 2"]) {
      expect(wide).toContain(counts);
      expect(narrow).toContain(counts);
    }
  });

  test("the unread divider survives, anchored to its event id", () => {
    for (const cols of [WIDE, NARROW]) {
      expect(routes.channel(cols).text()).toContain("● new");
    }
  });

  test("the channel list keeps unread, mentions and agent activity at 60", () => {
    const narrow = routes.channels(NARROW).text();
    expect(narrow).toContain("#engineering");
    expect(narrow).toContain("8");
    // IA §5.3's ambient signal is a status suffix, so it survives narrowing —
    // which is the whole point of surfacing it there rather than in a pane.
    expect(narrow).toContain("⚡");
  });

  test("the drawer's key hints wrap rather than truncate at 60 [G14]", () => {
    const narrow = routes.drawer(NARROW).text();
    // "exits stay discoverable at every width" — the escape hatch must be
    // present in full at exactly the width where it is most needed.
    expect(narrow).toContain("esc close");
    expect(narrow).toContain("↑/↓ select");
    expect(narrow).toContain("⏎ expand");
    expect(narrow).toContain("→ jump in");
  });

  test("the statusline stays exactly three rows at both widths [G11]", () => {
    for (const cols of [WIDE, NARROW]) {
      const rows = routes.channel(cols).screen();
      // Rows H-3..H-1 are the band; the row above them is the bottom rule.
      const rule = rows[rows.length - 4] ?? "";
      expect(rule.trim()).toMatch(/^─+$/);
    }
  });

  test("the connection glyph never drops, even at 40 columns", () => {
    // §2.1 row 1: "connection glyph never drops". At 40 the host, identity and
    // scope are all gone and the glyph is still there.
    const s = Session.open("seeded-basic", 40, 20)
      .goTo("Channels")
      .key("right");
    expect(s.text()).toContain("◉");
  });

  test("the breadcrumb elides from the left, keeping the current location", () => {
    const deep = routes.thread(NARROW).text();
    // §1.2: "the current location survives, the path is what is sacrificed."
    expect(deep).toContain("⤷");
    expect(deep).toContain("…");
  });
});

describe("loss states are visible, never silence (§1.3 property 3)", () => {
  test("a keyless daemon is distinguishable from a healthy one (§2.5)", () => {
    const keyless = routes.keyless(WIDE).text();
    const healthy = routes.home(WIDE).text();
    expect(keyless).toContain("keyless");
    expect(healthy).not.toContain("keyless");
  });

  test("a reconnecting relay shows the attempt, not a silent gap (§2.6)", () => {
    const s = Session.open("reconnect", WIDE, ROWS);
    expect(s.text()).toContain("◉ live");
    s.advance(500);
    // "these states look identical to 'hung' if you collapse them."
    expect(s.text()).toContain("retry 2");
    s.advance(2000);
    expect(s.text()).toContain("◉ live");
  });

  test("the empty scenario renders a legible screen, not a crash", () => {
    for (const cols of [WIDE, NARROW]) {
      const rows = routes.empty(cols).screen();
      expect(rows.length).toBeGreaterThan(0);
      expect(rows.join("\n")).toContain("PLACES");
    }
  });
});

describe("below the floor the app keeps running", () => {
  test("it renders one legible line rather than exiting or panicking", () => {
    const s = Session.open("seeded-basic", 20, 8);
    const rows = s.screen();
    expect(rows).toHaveLength(1);
    expect(rows[0]).toContain("too small");
  });

  test("a zero-size rect is a no-op, never a throw", () => {
    expect(() => Session.open("seeded-basic", 0, 0).screen()).not.toThrow();
    expect(Session.open("seeded-basic", 0, 0).screen()).toEqual([]);
  });
});

describe("a live turn advances the drawer's tail while you read (§2.4)", () => {
  test("frames arriving during a peek land in the transcript", () => {
    const s = Session.open("agent-stream", WIDE, ROWS)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("down");
    // Select claude-1 (the working agent, second on the [G12] ladder) and peek.
    s.key("down").key("return");
    expect(s.text()).toContain("Transcript");
    const before = s.text();
    s.advance(1200);
    // "The tail advances live while you read." A static peek would make the
    // drawer a screenshot rather than a window.
    expect(s.text()).not.toBe(before);
  });
});
