/**
 * Search — DESIGN.md §3.5 (query syntax retained) under NAVIGATION.md §7
 * (surface superseded: `ctrl+k` → L1 RESULTS → teleport).
 *
 * The operator-parsing cases are §3.5's stated rules. Each one is called out in
 * the document because the obvious implementation is subtly wrong, and each
 * failure mode is silent — a misparsed query returns the wrong rows rather than
 * an error, so "search is broken" is what the user concludes.
 */

import { describe, expect, test } from "bun:test";
import { parseQuery } from "../../src/layers/search";
import { Session } from "../helpers/drive";

describe("§3.5 operators parse at token boundaries, deliberately not \\b", () => {
  test("the four operators are extracted", () => {
    const q = parseQuery(
      "from:matt in:#engineering after:2026-07-01 read-state",
    );
    expect(q.from).toBe("matt");
    expect(q.in).toBe("engineering");
    expect(q.after).toBe(Date.parse("2026-07-01T00:00:00.000Z"));
    expect(q.text).toBe("read-state");
  });

  test("`built-in:react` is not an `in:` filter", () => {
    // A `\b` here would silently turn a hyphenated word into a channel filter
    // and return nothing — which reads as "search is broken" rather than as
    // "your query was reinterpreted".
    const q = parseQuery("built-in:react hooks");
    expect(q.in).toBeUndefined();
    expect(q.text).toBe("built-in:react hooks");
  });

  test("a URL containing `in:` is not a filter either", () => {
    const q = parseQuery("https://x.com/in:foo");
    expect(q.in).toBeUndefined();
    expect(q.text).toContain("https://x.com/in:foo");
  });

  test("an invalid operator value stays in the FTS text rather than erroring", () => {
    // `after:yesterday` should search for the words, not refuse the query.
    const q = parseQuery("after:yesterday slots");
    expect(q.after).toBeUndefined();
    expect(q.text).toBe("after:yesterday slots");
  });

  test("`before:` excludes the named day, matching Slack", () => {
    // NIP-01's `until` is inclusive and Slack is not, so `before:` is one
    // second before start-of-day. Omitting the adjustment silently includes a
    // whole extra day of results.
    const q = parseQuery("before:2026-07-02");
    expect(q.before).toBe(Date.parse("2026-07-02T00:00:00.000Z") - 1000);
  });

  test("`after:` is start-of-day inclusive", () => {
    const q = parseQuery("after:2026-07-01");
    expect(q.after).toBe(Date.parse("2026-07-01T00:00:00.000Z"));
  });

  test("a bare `in:` with no value is text, not an empty filter", () => {
    expect(parseQuery("in:").in).toBeUndefined();
  });

  test("`#` is optional on a channel filter", () => {
    expect(parseQuery("in:#engineering").in).toBe("engineering");
    expect(parseQuery("in:engineering").in).toBe("engineering");
  });
});

describe("§1.4 ctrl+k → L1 RESULTS → teleport", () => {
  test("ctrl+k enters a layer, not a modal — so ← returns to where you were", () => {
    const s = Session.open("seeded-basic", 100, 24)
      .goTo("Channels")
      .key("right")
      .key("right");
    expect(s.layer()).toBe("channel");
    s.key("k", { ctrl: true });
    expect(s.layer()).toBe("results");
    s.key("left");
    expect(s.layer()).toBe("channel");
  });

  test("an empty query shows a prompt, not 'no results'", () => {
    // Telling someone their empty search found nothing is a false negative, and
    // rendering every message in the community as a "result" is worse.
    const s = Session.open("seeded-basic", 100, 24).key("k", { ctrl: true });
    expect(s.text()).toContain("type to search");
  });

  test("typing filters as you type (§3.5)", () => {
    const s = Session.open("seeded-basic", 100, 24)
      .key("k", { ctrl: true })
      .type("slots");
    expect(s.text()).toContain("read-state slots cap at 8");
    expect(s.text()).not.toContain("bumped the pool ceiling");
  });

  test("[G8] there is exactly one ❯ while typing a query", () => {
    // Search is the one layer whose composer is always non-empty in normal use,
    // so a picker that kept `❯` on its row would break [G8] continuously rather
    // than in a corner case.
    const s = Session.open("seeded-basic", 100, 24)
      .key("k", { ctrl: true })
      .type("slots");
    expect(s.focusGlyphCount()).toBe(1);
    expect(s.text()).toContain("▌");
  });

  test("→ teleports to L2 at the hit, seeding the back stack [G15]", () => {
    const s = Session.open("seeded-basic", 100, 24)
      .key("k", { ctrl: true })
      .type("slots")
      .key("right");
    expect(s.layer()).toBe("channel");
    // The crumb records the route, so `←` returns to the results you were
    // reading rather than to a channel list you never opened.
    expect(s.crumb()).toBe("home › search › #engineering");
    expect(s.state.stack.entries.at(-1)?.eventId).toBe("ev_eng_002");
  });

  test("← returns to the results with the query intact", () => {
    const s = Session.open("seeded-basic", 100, 24)
      .key("k", { ctrl: true })
      .type("slots")
      .key("right")
      .key("left");
    expect(s.layer()).toBe("results");
    // The query is a draft on the results layer, so §5.4's per-layer draft
    // persistence is what brings it back.
    expect(s.state.composer).toBe("slots");
    expect(s.text()).toContain("read-state slots cap at 8");
  });

  test("re-filtering resets the selection so → never teleports nowhere", () => {
    const s = Session.open("seeded-basic", 100, 24)
      .key("k", { ctrl: true })
      .type("e");
    s.key("down").key("down").key("down");
    // Narrowing to one hit must bring the cursor back in range.
    s.type("ngineering topic that matches nothing");
    s.key("right");
    expect(s.layer()).toBe("results");
  });

  test("an operator query narrows by author", () => {
    const s = Session.open("seeded-basic", 100, 24)
      .key("k", { ctrl: true })
      .type("from:ana pool");
    expect(s.text()).toContain("bumped the pool ceiling");
    expect(s.text()).not.toContain("the lazy pool landed");
  });
});
