/**
 * T0 — the wire decoders (DESIGN.md §2.4, §2.6, §3.4.1).
 *
 * Every case here is a fact about a shape the daemon actually produces, taken
 * either from `crates/buzz-daemon/tests/api.rs` (which asserts the same bodies
 * over a real socket) or from `tui/fixtures/wire/`, which the daemon lane
 * generates from its own signing and encryption paths.
 *
 * **No daemon runs.** The decoders are pure, which is the point of splitting
 * them out of the fetch path: a decode bug that only a live daemon could
 * surface would be a bug nobody sees until dogfood.
 */

import { describe, expect, test } from "bun:test";
import {
  advancesCursor,
  decodeAgent,
  decodeChannel,
  decodeConnection,
  decodeMentionCandidate,
  decodeMessage,
  decodePresence,
  decodeSession,
  decodeStreamFrame,
  decodeTimelineRow,
  decodeUsage,
} from "../../src/client/wire";

const WIRE = new URL("../../fixtures/wire/", import.meta.url).pathname;

/** Load a wire fixture — the same files `tests/fixtures.rs` writes. */
function wire(name: string): Record<string, unknown> {
  return JSON.parse(
    require("node:fs").readFileSync(`${WIRE}${name}.json`, "utf8"),
  ) as Record<string, unknown>;
}

describe("connection state, carried verbatim (§2.6)", () => {
  test("reconnecting keeps attempt and countdown", () => {
    // `tests/api.rs::the_session_endpoint_surfaces_the_connection_state_verbatim`
    // asserts exactly this body over a real socket.
    expect(
      decodeConnection({
        state: "reconnecting",
        attempt: 3,
        next_retry_in_ms: 4000,
      }),
    ).toEqual({ state: "reconnecting", attempt: 3, next_retry_in_ms: 4000 });
  });

  test("auth failure keeps its reason, so remediation can be inline", () => {
    expect(
      decodeConnection({ state: "auth_failed", reason: "oa_expired" }),
    ).toEqual({ state: "auth_failed", reason: "oa_expired" });
  });

  test("an unknown state decodes to a loss state, never to connected", () => {
    // The conservative direction is the only safe one: guessing `connected`
    // would render a live dot over a dead socket, which §1.3 property 3 calls
    // the worst failure mode in the product.
    expect(decodeConnection({ state: "quantum_entangled" }).state).toBe(
      "disconnected",
    );
    expect(decodeConnection(null).state).toBe("disconnected");
    expect(decodeConnection(undefined).state).toBe("disconnected");
  });
});

describe("presence: unknown is never collapsed into offline (§2.4)", () => {
  test("the four states round-trip", () => {
    for (const state of ["present", "waking", "offline", "unknown"]) {
      expect(decodePresence(state)).toBe(state as never);
    }
  });

  test("an unrecognized presence is unknown, not offline", () => {
    expect(decodePresence("hibernating")).toBe("unknown");
    expect(decodePresence(undefined)).toBe("unknown");
  });
});

describe("session (§2.5)", () => {
  test("keyless is visible: absent archiving decodes false", () => {
    // "A keyless daemon must never look identical to a healthy one." Defaulting
    // `archiving` to true would do exactly that, so absence means keyless.
    expect(
      decodeSession({}, { relayUrl: "wss://r", communityName: "c" }),
    ).toMatchObject({ archiving: false });
  });

  test("the daemon's relay_url wins over the local fallback", () => {
    const session = decodeSession(
      { pubkey: "ab".repeat(32), relay_url: "wss://real", archiving: true },
      { relayUrl: "wss://guess", communityName: "c" },
    );
    expect(session.relayUrl).toBe("wss://real");
    expect(session.archiving).toBe(true);
  });

  test("a nameless identity falls back to its pubkey prefix, not 'unknown'", () => {
    // Two unnamed identities must stay distinguishable, which is the same
    // reasoning `mentions::Profile::label` uses on the daemon side.
    const session = decodeSession(
      { pubkey: "abcdef01".repeat(8) },
      { relayUrl: "", communityName: "" },
    );
    expect(session.name).toBe("abcdef01");
  });
});

describe("channels (channels::Channel)", () => {
  test("the snake_case wire fields land on the view type", () => {
    const channel = decodeChannel({
      id: "11111111-1111-1111-1111-111111111111",
      name: "engineering",
      topic: "relay + desktop",
      channel_type: "channel",
      member_count: 2,
      unread: 4,
      mentions: 2,
      agents_working: ["claude-1"],
      archived: false,
    });
    expect(channel).toMatchObject({
      name: "engineering",
      unread: 4,
      mentions: 2,
      agentsWorking: ["claude-1"],
      kind: "channel",
    });
  });

  test("channel_type 'unknown' is omitted rather than guessed as 'channel'", () => {
    // `channels.rs` keeps them distinct for the same reason presence does:
    // "we have not heard yet" and "it is a regular channel" are different
    // facts, and the second one is a guess.
    const channel = decodeChannel({ id: "x", channel_type: "unknown" });
    expect(channel?.kind).toBeUndefined();
  });

  test("a row with no id is dropped rather than rendered anonymously", () => {
    expect(decodeChannel({ name: "orphan" })).toBeUndefined();
  });
});

describe("fleet rows (fleet::FleetRow)", () => {
  const NOW = 1_785_852_720_000;

  test("blocked maps to needsInput — the [G12] ladder's top rung", () => {
    const agent = decodeAgent(
      { pubkey: "pk", name: "goose-1", state: "blocked" },
      NOW,
    );
    expect(agent?.state).toBe("needsInput");
    expect(agent?.presence).toBe("present");
  });

  test("elapsed_secs becomes a turn start, so the badge keeps counting", () => {
    // The daemon reports elapsed; the badge needs a start. Storing elapsed
    // would freeze the badge between polls, which is precisely the number an
    // operator watching a stuck turn is reading.
    const agent = decodeAgent(
      { pubkey: "pk", state: "working", turn: "4a91", elapsed_secs: 252 },
      NOW,
    );
    expect(agent?.turnStartedAt).toBe(NOW - 252_000);
    expect(agent?.turnId).toBe("4a91");
  });

  test("offline and unknown do not collapse into one presence", () => {
    expect(decodeAgent({ pubkey: "a", state: "offline" }, NOW)?.presence).toBe(
      "offline",
    );
    expect(decodeAgent({ pubkey: "b", state: "unknown" }, NOW)?.presence).toBe(
      "unknown",
    );
  });
});

describe("messages", () => {
  test("created_at seconds become ts milliseconds", () => {
    // The one unit conversion in the transport, done once here so no renderer
    // has to know which unit it received.
    const events = wire("window-page").events as Array<Record<string, unknown>>;
    const first = events[0] as Record<string, unknown>;
    const message = decodeMessage({
      ...first,
      channel_id: wire("window-page").channel_id,
    });
    expect(message?.ts).toBe((first.created_at as number) * 1000);
    expect(message?.content).toBe("read-state slots are the risky part");
  });

  test("a message with no id is dropped — the unread divider anchors to one", () => {
    expect(decodeMessage({ channel_id: "c", content: "hi" })).toBeUndefined();
    expect(decodeMessage({ id: "e", content: "hi" })).toBeUndefined();
  });

  /**
   * **M3 regression.** `GET /channel/{id}/message` serves `timeline::TimelineRow`
   * — `{event, thread}`, with the relay's signed event verbatim inside — which
   * is **not** the flat shape `decodeMessage` reads. Feeding a history row to
   * `decodeMessage` returns `undefined` for every row, because `channel_id` is a
   * tag on the inner event rather than a field on the outer object. That is
   * silent, and it looks exactly like an empty channel.
   */
  describe("timeline rows are a different shape from hydrated messages", () => {
    const channelId = wire("window-page").channel_id as string;
    const events = wire("window-page").events as Array<Record<string, unknown>>;

    test("the nested event decodes, with seconds converted once", () => {
      const first = events[0] as Record<string, unknown>;
      const row = decodeTimelineRow({ event: first, thread: null }, channelId);
      expect(row?.id).toBe(first.id as string);
      expect(row?.channelId).toBe(channelId);
      expect(row?.ts).toBe((first.created_at as number) * 1000);
      expect(row?.content).toBe("read-state slots are the risky part");
      // No profile join on a history page, so the short pubkey is the honest
      // label — inventing a name would make unresolved look resolved.
      expect(row?.author.name).toBe((first.pubkey as string).slice(0, 8));
    });

    test("the flat decoder cannot read this shape — which is why it exists", () => {
      // The exact confusion that produced 28 blank rows against a daemon
      // serving real messages. Pinned so the two decoders cannot be swapped
      // back for one another without a red test.
      expect(decodeMessage({ event: events[0], thread: null })).toBeUndefined();
    });

    test("a thread overlay contributes its reply count; null contributes none", () => {
      const first = events[0] as Record<string, unknown>;
      expect(
        decodeTimelineRow(
          { event: first, thread: { reply_count: 4 } },
          channelId,
        )?.replyCount,
      ).toBe(4);
      expect(
        decodeTimelineRow({ event: first, thread: null }, channelId)
          ?.replyCount,
      ).toBeUndefined();
    });

    test("a row with no event id is dropped, like a message with none", () => {
      expect(
        decodeTimelineRow(
          { event: { content: "hi" }, thread: null },
          channelId,
        ),
      ).toBeUndefined();
      expect(decodeTimelineRow({ thread: null }, channelId)).toBeUndefined();
    });
  });
});

describe("usage: null means not reported, never zero (§3.4.1)", () => {
  test("the decoded NIP-AM fixture round-trips with its window", () => {
    const decoded = wire("agent-usage").decoded as Record<string, unknown>;
    const usage = decodeUsage(decoded);
    expect(usage).toMatchObject({
      inTokens: 12_480,
      outTokens: 1932,
      cacheRead: 9120,
      costUsd: 0.41,
      model: "claude-opus-5",
      contextWindow: 200_000,
    });
  });

  test("an absent context window stays null so the renderer shows no bar", () => {
    // "a client-side model table is precisely the thing that rots, so an absent
    // window renders `ctx 58,204 / —` with **no bar**."
    const usage = decodeUsage({ tokens_in: 10, tokens_out: 5 });
    expect(usage?.contextWindow).toBeNull();
    expect(usage?.contextUsed).toBe(15);
  });

  test("an agent that reported nothing has null usage, not zeroes", () => {
    const usage = decodeUsage({ model: "m" });
    expect(usage?.inTokens).toBeNull();
    expect(usage?.outTokens).toBeNull();
    // And `contextUsed` too: summing two unreported halves to 0 would be a
    // fabricated measurement the renderer would faithfully display.
    expect(usage?.contextUsed).toBeNull();
  });
});

describe("mention candidates carry pubkeys ([D-2])", () => {
  test("the resolved pubkey survives decoding", () => {
    // `tests/api.rs::mention_candidates_carry_pubkeys_and_the_cap` asserts the
    // same body from the daemon.
    const candidate = decodeMentionCandidate({
      pubkey: "ab".repeat(32),
      display_name: "matt",
      in_roster: true,
      is_agent: false,
    });
    expect(candidate?.pubkey).toBe("ab".repeat(32));
    expect(candidate?.displayName).toBe("matt");
  });

  test("a candidate with no pubkey is dropped, never rendered by name alone", () => {
    // [D-2] is "what you picked is what gets tagged, by construction". A
    // candidate with no pubkey could only be tagged by re-resolving the name,
    // which is the second implementation [D-2] exists to delete.
    expect(decodeMentionCandidate({ display_name: "matt" })).toBeUndefined();
  });
});

describe("stream frames", () => {
  test("both `data` and `payload` are accepted", () => {
    // `daemon-api.md` §4.2 writes `data`; `stream.rs` serializes `payload`.
    // Accepting one would drop every frame the other side sends.
    expect(
      decodeStreamFrame({ seq: 1, type: "message.new", data: { a: 1 } })?.data,
    ).toEqual({ a: 1 });
    expect(
      decodeStreamFrame({ seq: 2, type: "message.new", payload: { b: 2 } })
        ?.data,
    ).toEqual({ b: 2 });
  });

  test("a frame with no type is dropped rather than advancing the cursor", () => {
    expect(decodeStreamFrame({ seq: 9 })).toBeUndefined();
  });

  test("control frames do not advance the cursor", () => {
    // `stream.reset` carries no seq (`StreamControl` serializes only `type`).
    // Advancing on it would rewind the cursor to 0 and replay the whole ring.
    const reset = decodeStreamFrame({ type: "stream.reset" });
    expect(reset).toBeDefined();
    expect(reset && advancesCursor(reset)).toBe(false);

    const data = decodeStreamFrame({ seq: 42, type: "message.new" });
    expect(data && advancesCursor(data)).toBe(true);
  });

  test("a data frame with seq 0 does not advance either", () => {
    // Seq is 1-based on the daemon (`stream.rs` increments before assigning),
    // so a 0 means the field was absent — and treating an absent cursor as a
    // real one would make the next reconnect replay from the ring's start.
    const frame = decodeStreamFrame({ type: "message.new" });
    expect(frame && advancesCursor(frame)).toBe(false);
  });
});
