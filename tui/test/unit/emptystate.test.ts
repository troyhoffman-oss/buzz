/**
 * Empty lists must say *why* they are empty — DESIGN.md §1.3 property 3.
 *
 * Every one of these cases was found by running the compiled TUI against a real
 * daemon holding a real identity, whose stores are empty because the relay
 * ingest is not built yet (dogfood M2). Nothing in the fixture suite could
 * catch them: every fixture is populated, so the zero-row branch of each list
 * layer had never once rendered.
 *
 * That is the reusable lesson, and it is why these tests assert on the *cause*
 * rather than on the row count — a test that only checked "some text appears"
 * would pass against the blank pane if the pane happened to contain a
 * breadcrumb.
 */

import { describe, expect, test } from "bun:test";
import { renderScreen } from "../../src/app/screen";
import { initialState } from "../../src/app/state";
import { withDefaultSelection } from "../../src/app/dispatch";
import { emptyStateRows } from "../../src/render/emptystate";
import { type StyledRow, rowText } from "../../src/render/span";
import { displayWidth } from "../../src/render/width";
import type { Snapshot } from "../../src/client/types";
import {
  WAVE_2_TAG,
  buildHomeRows,
  descendTarget,
  placeIsShipped,
} from "../../src/layers/home";
import { FIXED_NOW, Session } from "../helpers/drive";

/** A snapshot with nothing in it — the shape a live Wave-1 daemon returns. */
function emptySnapshot(overrides: Partial<Snapshot> = {}): Snapshot {
  return {
    session: {
      pubkey:
        "98071e8cad3578dd8b20579d25aaca4f3ffc60bfa66f90b6191336e52b63d2b4",
      name: "98071e8c",
      relayUrl: "wss://relay.example",
      communityName: "buzz",
      connection: { state: "disconnected" },
      archiving: true,
    },
    communities: [],
    channels: [],
    agents: [],
    attention: [],
    threads: [],
    huddles: [],
    messages: {},
    transcripts: {},
    usage: {},
    mentionCandidates: [],
    ...overrides,
  };
}

/**
 * The text an empty-state row draws.
 *
 * These cases are about *which words* an empty list says — §1.3 property 3's
 * whole point is that the three causes must not read alike — so they assert on
 * the projection, exactly as they did before the rows gained styling. The
 * colour reinforces the distinction; the words are what carry it, and asserting
 * on them is what keeps a restyle from ever turning one of these red.
 */
const text = (row: StyledRow): string => rowText(row);

/** A rendered block, joined — what most of these cases match against. */
const joined = (rows: readonly StyledRow[]): string =>
  rows.map(text).join("\n");

describe("emptyStateRows — the three causes are distinguishable", () => {
  test("an unmounted endpoint names the endpoint, not the community", () => {
    const rows = emptyStateRows(
      "channels",
      "/channel",
      { connection: { state: "connected" }, missing: ["/channel"] },
      80,
    );
    expect(joined(rows)).toContain("/channel");
    expect(joined(rows)).toContain("upgrade");
  });

  test("an unmounted endpoint outranks a bad connection", () => {
    // Both are true at once here. The endpoint wins because a disconnected
    // daemon fills in on its own and an unmounted route never does — telling
    // the operator to wait would be advice that cannot come true.
    const rows = emptyStateRows(
      "channels",
      "/channel",
      { connection: { state: "disconnected" }, missing: ["/channel"] },
      80,
    );
    expect(joined(rows)).toContain("does not serve");
    expect(joined(rows)).not.toContain("fills in once");
  });

  test("a disconnected daemon says so and promises the list will fill", () => {
    const rows = emptyStateRows(
      "channels",
      "/channel",
      { connection: { state: "disconnected" }, missing: [] },
      80,
    );
    expect(joined(rows)).toContain("not connected to the relay");
    expect(joined(rows)).toContain("fills in once the relay connects");
  });

  test("each non-connected state gets its own reason, not one generic line", () => {
    // The point of §1.3 property 3 is that states are *told apart*. A single
    // "not connected" string for all seven would pass a laxer test and defeat
    // the requirement.
    const reason = (connection: Parameters<typeof emptyStateRows>[2]) =>
      joined(emptyStateRows("channels", "/channel", connection, 100));

    const seen = new Set(
      (
        [
          { state: "disconnected" },
          { state: "connecting" },
          { state: "authenticating" },
          { state: "reconnecting", attempt: 2, next_retry_in_ms: 500 },
          { state: "rate_limited", retry_after_ms: 1000 },
          { state: "dns_brownout" },
          { state: "auth_failed", reason: "bad tag" },
        ] as const
      ).map((connection) => reason({ connection, missing: [] })),
    );
    expect(seen.size).toBe(7);
  });

  test("connected and genuinely empty is the only benign line", () => {
    const rows = emptyStateRows(
      "channels",
      "/channel",
      { connection: { state: "connected" }, missing: [] },
      80,
    );
    expect(rows).toHaveLength(1);
    // `joined` rather than indexing: the assertion is "the benign case says
    // this and does not mention the relay", which is a statement about the
    // whole block, and the length check above already pins it to one row.
    expect(joined(rows)).toContain("no channels");
    expect(joined(rows)).not.toContain("relay");
  });

  test("every row is padded to the requested width", () => {
    // A short row leaves the previous frame's pixels behind it on a real
    // terminal, which is how an empty-state message ends up superimposed on
    // whatever was there before.
    for (const cols of [60, 80, 120]) {
      for (const rows of [
        emptyStateRows(
          "channels",
          "/channel",
          { connection: { state: "disconnected" }, missing: [] },
          cols,
        ),
        emptyStateRows(
          "agents",
          "/agent/fleet",
          { connection: { state: "connected" }, missing: ["/agent/fleet"] },
          cols,
        ),
      ]) {
        for (const row of rows) expect(displayWidth(text(row))).toBe(cols);
      }
    }
  });
});

describe("the list layers render it — measured against a live daemon", () => {
  /** Render one layer of an empty snapshot, as the shell would. */
  function screenAt(layerKind: "channels" | "agents", snapshot: Snapshot) {
    let state = withDefaultSelection(initialState(snapshot));
    state = {
      ...state,
      stack: {
        entries: [
          ...state.stack.entries,
          { kind: layerKind, crumb: layerKind, selection: 0 },
        ],
      },
    };
    return renderScreen(state, 120, 36, FIXED_NOW).join("\n");
  }

  test("L1 CHANNELS is not a blank pane against a disconnected daemon", () => {
    // The bug, verbatim: 30 blank rows, indistinguishable from a healthy but
    // empty community and from an unmounted endpoint.
    const text = screenAt("channels", emptySnapshot());
    expect(text).toContain("no channels");
    expect(text).toContain("not connected to the relay");
  });

  test("L1 CHANNELS names the endpoint when the daemon does not mount it", () => {
    const text = screenAt(
      "channels",
      emptySnapshot({
        session: {
          ...emptySnapshot().session,
          connection: { state: "connected" },
        },
        missing: ["/channel"],
      }),
    );
    expect(text).toContain("/channel");
  });

  test("L1 AGENTS is not a blank pane either", () => {
    const text = screenAt("agents", emptySnapshot());
    expect(text).toContain("no agents");
    expect(text).toContain("not connected to the relay");
  });

  test("[G8] exactly one focus glyph survives an empty list", () => {
    // Found by counting glyphs in the live captures after the first fix
    // landed: L1 CHANNELS and L1 AGENTS rendered **zero** `❯`. The rule is
    // "the list owns it while the composer is empty" — and a list with no rows
    // has nowhere to put it. Zero is worse than two here: it says the keyboard
    // goes nowhere, on the exact screen someone is trying to type into.
    for (const layer of ["channels", "agents"] as const) {
      const text = screenAt(layer, emptySnapshot());
      const glyphs = text.split("").filter((c) => c === "\u276f").length;
      expect(glyphs).toBe(1);
    }
  });

  test("[G8] holds on a populated list too — the fallback is not always-on", () => {
    // The inverse. A fallback that fired unconditionally would put `❯` on the
    // composer *and* leave the list marking a row, which is the two-glyph
    // failure the same rule forbids.
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right");
    expect(s.focusGlyphCount()).toBe(1);
    expect(s.focusRow()).not.toContain("filter channels");
  });

  test("a filter that matches nothing does not accuse the transport", () => {
    // Two different emptinesses. A query that missed is a statement about the
    // query; blaming the relay for it would send the operator to debug a
    // healthy connection.
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right").type("zzzznotachannel");
    expect(s.text()).toContain("no channels match");
    // Scoped to the body: the statusline legitimately prints the relay URL on
    // every frame, so asserting over the whole screen would be asserting on
    // chrome. The claim is that the *list* does not blame the transport.
    const body = s.screen().slice(0, 24).join("\n");
    expect(body).not.toContain("relay");
    expect(body).not.toContain("connect");
  });
});

describe("§4.1.3 — an unshipped destination is tagged, never silent", () => {
  test("Settings and Me carry the Wave 2 tag", () => {
    const rows = Session.open("seeded-basic").text().split("\n");
    expect(rows.find((r) => r.includes("Settings"))).toContain("Wave 2");
    expect(rows.find((r) => /\bMe\b/.test(r))).toContain("Wave 2");
  });

  test("the shipped places carry no tag", () => {
    // Guards the inverse: a tag that leaked onto `Channels` would teach the
    // roadmap wrong, which is worse than teaching nothing.
    const s = Session.open("seeded-basic");
    const rows = s.text().split("\n");
    expect(rows.find((r) => r.includes("Channels"))).not.toContain("Wave 2");
    expect(rows.find((r) => r.includes("Agents"))).not.toContain("Wave 2");
  });

  test("the tag and the descent come from one predicate, for every place", () => {
    // The drift guard. Two hand-maintained lists — one deciding which rows get
    // the tag, one deciding which rows descend — would eventually disagree,
    // and the bad direction is silent: a row tagged `Wave 2` that descends
    // anyway teaches the roadmap wrong. Swept over every PlaceId rather than
    // spot-checked, so a fifth place cannot be added to only one of them.
    const rows = buildHomeRows({
      communities: [],
      attention: [],
      channels: [],
      agentsWorking: 0,
      collapsedGroups: new Set(),
      collapsedZones: new Set(),
    });
    const places = rows.filter((r) => r.kind === "place");
    expect(places.length).toBeGreaterThan(0);
    for (const row of places) {
      if (row.kind !== "place") continue;
      const shipped = placeIsShipped(row.place);
      expect(row.status === WAVE_2_TAG).toBe(!shipped);
      expect(descendTarget(row) !== null).toBe(shipped);
    }
  });

  test("→ on a Wave-2 place stays put rather than descending", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Settings").key("right");
    expect(s.layer()).toBe("home");
  });
});
