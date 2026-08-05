/**
 * T0 units for the render pieces whose rules live in DESIGN.md rather than in
 * NAVIGATION.md — §2.1's statusline drop order, §3.1's grouping and dividers,
 * §3.4.1's usage rules, §2.3's drawer degradation.
 *
 * These are reachable through `renderScreen`, but each encodes a *stated rule*
 * whose violation is legible only up close. A snapshot proves the screen looks
 * right today; these prove the reason it does.
 */

import { describe, expect, test } from "bun:test";
import type { Agent, Message, Usage } from "../../src/client/types";
import {
  ATTENTION_LADDER,
  buildDrawerRows,
  fitDrawerRows,
  expansionHeight,
  liveSummary,
} from "../../src/render/drawer";
import {
  connectionGlyph,
  renderMeter,
  row1,
  row2,
  row3,
  METER_CELLS,
} from "../../src/render/statusline";
import {
  GROUPING_WINDOW_MS,
  continuesGroup,
  renderTimeline,
  threadRootIds,
} from "../../src/render/timeline";
import { burnRate, usageStrip, sortFleet } from "../../src/layers/agents";
import { MS_PER_MINUTE } from "../../src/time/units";
import { type StyledRow, rowText } from "../../src/render/span";
import { displayWidth } from "../../src/render/width";

const T0 = Date.parse("2026-08-04T14:12:00.000Z");

/**
 * The text a styled row draws — what every rule in this file is *about*.
 *
 * The renderers emit spans now, so a colour decision and a drop-order decision
 * live in the same return value. These cases are about the second: §2.1's drop
 * order, §3.1's grouping, §2.3's degradation are all statements about which
 * *characters* survive a narrowing, and they were written against the strings
 * the terminal receives. Projecting here keeps them testing exactly that, so a
 * restyle can never turn one of these red and a genuine drop-order regression
 * can never hide behind one.
 */
const text = (row: StyledRow): string => rowText(row);

const msg = (over: Partial<Message> = {}): Message => ({
  id: "ev_1",
  channelId: "ch",
  author: { pubkey: "pk_a", name: "ana", isAgent: false },
  ts: T0,
  content: "hello",
  ...over,
});

describe("§2.1 statusline drop order", () => {
  const state = {
    relayUrl: "buzz://relay.example",
    identity: "troy",
    scope: "#engineering",
    connection: { state: "connected" } as const,
    archiving: true,
    meter: 0.41,
    unread: 12,
    mentions: 3,
    dms: 2,
    agentsWorking: 3,
    huddles: 1,
    chatLayer: true,
  };

  test("row 1 drops the host first and the connection glyph never", () => {
    // "host truncates first; **connection glyph never drops**". A chat client
    // that looks idle while its socket is dead is the worst failure mode in
    // this product (§1.3 property 3), so the glyph is the one thing the row
    // cannot lose.
    expect(text(row1(state, 80))).toContain("buzz://relay.example");
    const narrow = text(row1(state, 40));
    expect(narrow).not.toContain("buzz://relay.example");
    expect(narrow).toContain("◉ live");
    expect(text(row1(state, 20))).toContain("◉");
  });

  test("row 2 keeps mentions and DMs; the unread total goes last", () => {
    // A generic right-to-left drop would take DMs first, inverting the table.
    const narrow = text(row2(state, 40));
    expect(narrow).toContain("3 mentions");
    expect(narrow).toContain("2 DMs");
    expect(narrow).not.toContain("12 unread");
  });

  test("row 3 is the [G13] door label and swaps hint for live count", () => {
    // "The same row swaps a static hint for a live count, and that count is
    // exactly what `↓` opens."
    expect(text(row3(state, 80))).toContain("3 agents working");
    expect(text(row3(state, 80))).toContain("↓ live");
    const idle = text(row3({ ...state, agentsWorking: 0, huddles: 0 }, 80));
    expect(idle).toContain("↓ nothing running");
  });

  test("row 3 advertises the arrows only where they are doors (§2.3)", () => {
    // On a picker layer the arrows already move the list, so naming them as
    // doors would be a lie.
    const picker = text(row3({ ...state, chatLayer: false }, 80));
    expect(picker).toContain("↑↓ to navigate");
    expect(picker).not.toContain("↓ live");
  });

  test("row 3 keeps the counts and drops the prose when narrowed", () => {
    // "never truncated below the affordance count" — the count *is* the
    // affordance.
    const narrow = text(row3(state, 34));
    expect(narrow).toContain("3 agents working");
  });

  test("a keyless daemon is visible even though it is not a connection state", () => {
    expect(text(row1({ ...state, archiving: false }, 100))).toContain("keyless");
  });

  test("every non-connected state gets a distinct glyph (§2.6)", () => {
    // "these states look identical to 'hung' if you collapse them."
    const glyphs = [
      connectionGlyph({ state: "connected" }),
      connectionGlyph({ state: "connecting" }),
      connectionGlyph({ state: "authenticating" }),
      connectionGlyph({
        state: "reconnecting",
        attempt: 2,
        next_retry_in_ms: 4000,
      }),
      connectionGlyph({ state: "rate_limited", retry_after_ms: 4200 }),
      connectionGlyph({ state: "dns_brownout" }),
      connectionGlyph({ state: "auth_failed", reason: "x" }),
      connectionGlyph({ state: "disconnected" }),
    ];
    expect(new Set(glyphs).size).toBe(glyphs.length);
  });

  test("the meter is a fixed 20-cell bar, atomic at every width", () => {
    // "atomic at every width" — a bar that shrinks is a bar whose fill you
    // cannot compare between two glances, which is the only thing it is for.
    for (const fill of [0, 0.41, 0.5, 1]) {
      const bar = renderMeter(fill);
      const inner = bar.slice(bar.indexOf("[") + 1, bar.indexOf("]"));
      expect(displayWidth(inner)).toBe(METER_CELLS);
    }
  });

  test("the meter clamps rather than overflowing", () => {
    expect(renderMeter(-1)).toContain("0%");
    expect(renderMeter(2)).toContain("100%");
  });
});

describe("§3.1 author grouping and dividers", () => {
  test("consecutive messages by one author inside the window collapse", () => {
    const first = msg({ id: "a", ts: T0 });
    const second = msg({ id: "b", ts: T0 + GROUPING_WINDOW_MS - 1 });
    expect(continuesGroup(first, second)).toBe(true);
  });

  test("a later message by the same author gets its own header", () => {
    const first = msg({ id: "a", ts: T0 });
    const later = msg({ id: "b", ts: T0 + GROUPING_WINDOW_MS + 1 });
    expect(continuesGroup(first, later)).toBe(false);
  });

  test("a different author always breaks the group", () => {
    const a = msg({ id: "a" });
    const b = msg({
      id: "b",
      author: { pubkey: "pk_b", name: "matt", isAgent: false },
    });
    expect(continuesGroup(a, b)).toBe(false);
  });

  test("a system row never groups under a human's header", () => {
    // §3.1 gives non-conversational kinds "their own dimmed rows"; folding one
    // under a human's header would attribute it to that human.
    const human = msg({ id: "a" });
    const system = msg({ id: "b", system: true, ts: T0 + 1000 });
    expect(continuesGroup(human, system)).toBe(false);
    expect(continuesGroup(system, msg({ id: "c", ts: T0 + 2000 }))).toBe(false);
  });

  test("the unread divider is anchored to an event id, not a row index", () => {
    // §3.1's most-tested piece of chat chrome: "It must survive a reconnect
    // burst, a re-render, and a tier change."
    const messages = [
      msg({ id: "a", ts: T0 - 3000 }),
      msg({ id: "b", ts: T0 - 2000 }),
      msg({ id: "c", ts: T0 - 1000 }),
    ];
    const before = renderTimeline(messages, {
      cols: 60,
      unreadAfterEventId: "b",
    });
    const dividerAt = before.findIndex((r) => text(r.text).includes("● new"));
    expect(dividerAt).toBeGreaterThan(0);

    // A burst arrives *before* the anchor. A row-index divider would drift; an
    // id-anchored one stays attached to the message it describes.
    const burst = [
      msg({ id: "x0", ts: T0 - 2500 }),
      msg({ id: "x1", ts: T0 - 2400 }),
      ...messages,
    ].sort((m, n) => m.ts - n.ts);
    const after = renderTimeline(burst, { cols: 60, unreadAfterEventId: "b" });
    const rowAfterDivider =
      after[after.findIndex((r) => text(r.text).includes("● new")) - 1];
    const rowBeforeBurst = before[dividerAt - 1];
    expect(rowAfterDivider?.messageId).toBe(rowBeforeBurst?.messageId ?? "");
  });

  test("thread roots are the messages with replies, at any depth", () => {
    const messages = [
      msg({ id: "a", replyCount: 4 }),
      msg({ id: "b" }),
      msg({ id: "c", replyTo: "a", replyCount: 2 }),
    ];
    // A reply that grew its own conversation is a root: the gesture is for
    // "pick a conversation", and `c` is one.
    expect(threadRootIds(messages)).toEqual(["a", "c"]);
  });

  test("a diff renders as its own rows, collapsed by default (§3.1)", () => {
    const rows = renderTimeline(
      [
        msg({
          id: "d",
          diff: {
            path: "crates/buzz-db/src/read_state.rs",
            added: 18,
            removed: 4,
            hunks: [{ oldLine: 86, newLine: null, kind: "remove", text: "x" }],
          },
        }),
      ],
      { cols: 80 },
    );
    const rendered = rows.map((r) => text(r.text)).join("\n");
    // "Collapsed to a header plus the changed-hunk summary" — a 200-line diff
    // inline is a timeline you have lost.
    expect(rendered).toContain("diff · crates/buzz-db/src/read_state.rs");
    expect(rendered).toContain("+18 −4");
    expect(rendered).toContain("⏎ expand");
    expect(rendered).not.toContain("│ x");
  });

  test("an expanded diff shows its hunks", () => {
    const rows = renderTimeline(
      [
        msg({
          id: "d",
          diff: {
            path: "a.rs",
            added: 1,
            removed: 1,
            hunks: [
              { oldLine: 86, newLine: null, kind: "remove", text: "old" },
              { oldLine: null, newLine: 86, kind: "add", text: "new" },
            ],
          },
        }),
      ],
      { cols: 80, expandedDiffs: new Set(["d"]) },
    );
    const rendered = rows.map((r) => text(r.text)).join("\n");
    expect(rendered).toContain("-old");
    expect(rendered).toContain("+new");
  });
});

describe("§2.3 drawer degradation order", () => {
  const agents: Agent[] = [
    {
      pubkey: "p1",
      name: "a1",
      runtime: "acp",
      presence: "present",
      state: "completed",
    },
    {
      pubkey: "p2",
      name: "a2",
      runtime: "acp",
      presence: "present",
      state: "working",
    },
    {
      pubkey: "p3",
      name: "a3",
      runtime: "acp",
      presence: "present",
      state: "needsInput",
    },
    {
      pubkey: "p4",
      name: "a4",
      runtime: "acp",
      presence: "present",
      state: "idle",
    },
  ];

  test("rows are in the [G12] ladder: needs-input > working > failed > completed", () => {
    // `idle` is not in §2.3's list, and it sits between `failed` and
    // `completed` here on purpose: an idle agent is one you *could* give work
    // to, while a completed one is finished with the work it had. Sorting
    // completed above idle would put the least actionable row higher, which is
    // the whole thing the ladder exists to prevent.
    const rows = buildDrawerRows(agents, [], [], []);
    expect(rows.map((r) => r.label)).toEqual(["a3", "a2", "a4", "a1"]);
    expect(ATTENTION_LADDER.needsInput).toBeLessThan(ATTENTION_LADDER.working);
    expect(ATTENTION_LADDER.working).toBeLessThan(ATTENTION_LADDER.failed);
    expect(ATTENTION_LADDER.failed).toBeLessThan(ATTENTION_LADDER.completed);
    expect(ATTENTION_LADDER.idle).toBeLessThan(ATTENTION_LADDER.completed);
  });

  test("completed and idle collapse into a count before any live row drops", () => {
    // §2.3's stated order. A naive "drop from the bottom" would take the
    // SYSTEM row before a completed agent, which is exactly backwards.
    const rows = buildDrawerRows(agents, [], [], ["relay reconnecting"]);
    const fitted = fitDrawerRows(rows, 5);
    expect(fitted.collapsed).toBeGreaterThan(0);
    const kept = fitted.rows.map((r) => r.label);
    expect(kept).toContain("a3");
    expect(kept).toContain("a2");
  });

  test("headers compact before a live row is dropped", () => {
    const rows = buildDrawerRows(agents, [], [], ["relay reconnecting"]);
    const fitted = fitDrawerRows(rows, 4);
    expect(fitted.compactHeaders).toBe(true);
    expect(fitted.rows.map((r) => r.label)).toContain("a3");
  });

  test("at least one live row survives, however tight", () => {
    // A drawer showing zero of three working agents is worse than one showing
    // one and saying so.
    const rows = buildDrawerRows(agents, [], [], []);
    const fitted = fitDrawerRows(rows, 1);
    expect(fitted.rows.length).toBeGreaterThanOrEqual(1);
    expect(fitted.rows[0]?.label).toBe("a3");
  });

  test("nothing is degraded when everything fits", () => {
    const rows = buildDrawerRows(agents, [], [], []);
    const fitted = fitDrawerRows(rows, 40);
    expect(fitted.collapsed).toBe(0);
    expect(fitted.compactHeaders).toBe(false);
    expect(fitted.rows).toHaveLength(rows.length);
  });

  test("the summary reports what is live, or says nothing is", () => {
    expect(liveSummary(buildDrawerRows(agents, [], [], []))).toContain(
      "agents working",
    );
    expect(liveSummary([])).toBe("nothing running");
  });

  test("§2.4 the expansion is capped at min(18, floor(H/2))", () => {
    // The `H/2` half fixes the measured Claude Code failure: a fixed 18 on a
    // 16-row terminal overflows, and the overflow evicts the chat the peek
    // exists to avoid leaving.
    expect(expansionHeight(16)).toBe(8);
    expect(expansionHeight(44)).toBe(18);
    expect(expansionHeight(100)).toBe(18);
    expect(expansionHeight(2)).toBe(1);
  });
});

describe("§3.4.1 usage rules an earlier draft's own mock violated", () => {
  const usage = (over: Partial<Usage> = {}): Usage => ({
    inTokens: 12480,
    outTokens: 1932,
    cacheRead: 9120,
    cacheWrite: null,
    costUsd: 0.0412,
    model: "claude-opus-5",
    contextUsed: 58204,
    contextWindow: null,
    ...over,
  });

  test("null means NOT REPORTED — render `—`, never `0`", () => {
    expect(usageStrip(usage({ contextUsed: null }))).toContain("—");
    expect(usageStrip(usage({ contextUsed: null }))).not.toMatch(/\b0\//);
  });

  test("an absent context window renders `—`, never a client-side default", () => {
    // "that 200,000 was a client-side model table, which is precisely the thing
    // that rots (opus-5 ships 200k *and* 1M variants)." Deriving a denominator
    // is the same sin as deriving `totalTokens`.
    const strip = usageStrip(usage());
    expect(strip).toContain("/—");
    expect(strip).not.toContain("200");
  });

  test("a provider-reported window is used when present", () => {
    expect(usageStrip(usage({ contextWindow: 200000 }))).toContain("200k");
  });

  test("cost shows only when costUsd is present", () => {
    expect(usageStrip(usage())).toContain("$0.04");
    expect(usageStrip(usage({ costUsd: null }))).not.toContain("$");
  });

  test("burn rate answers the question totals cannot", () => {
    // "'Is this agent stuck in a loop burning money' is what an operator
    // actually needs, and no total answers it."
    expect(burnRate(usage(), 10 * MS_PER_MINUTE)).toBe("1441 tok/min");
    expect(
      burnRate(usage({ inTokens: null, outTokens: null }), MS_PER_MINUTE),
    ).toBe("—");
    expect(burnRate(usage(), 0)).toBe("—");
  });

  test("the fleet sorts by the attention ladder, not by name", () => {
    const fleet = sortFleet([
      {
        pubkey: "z",
        name: "zeta",
        runtime: "r",
        presence: "present",
        state: "idle",
      },
      {
        pubkey: "a",
        name: "alpha",
        runtime: "r",
        presence: "present",
        state: "needsInput",
      },
    ]);
    // The orchestrator's question is "which of my agents needs me", not
    // "which is alphabetically first".
    expect(fleet[0]?.name).toBe("alpha");
  });
});
