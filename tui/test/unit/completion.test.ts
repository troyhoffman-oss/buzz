/**
 * T0 + driven tests for the completion band — NAVIGATION.md §2.5, DESIGN.md
 * §3.3 (retained verbatim by §7).
 *
 * §3.3's mechanics are ux-patterns P11/P12, and the cases below are its stated
 * rules rather than a sample of behaviour — every one of them is a rule the
 * document calls out because the obvious implementation gets it wrong.
 */

import { describe, expect, test } from "bun:test";
import {
  MENTION_CAP,
  applyCompletion,
  detectTrigger,
  mentionCapReached,
  rankCandidates,
  renderMentionPicker,
} from "../../src/render/completion";
import type { MentionCandidate } from "../../src/client/types";
import { Session } from "../helpers/drive";

describe("§3.3 trigger detection — a pure function of (text, cursor)", () => {
  test("rule 1: the nearest trigger backwards from the cursor", () => {
    const trigger = detectTrigger("hey @matt and @an", 17);
    expect(trigger?.at).toBe(14);
    expect(trigger?.query).toBe("an");
  });

  test("rule 2: `foo@bar` does not trigger", () => {
    // The `@` must be preceded by start-of-input or whitespace, or every email
    // address in a message opens a mention picker mid-sentence.
    expect(detectTrigger("mail me at troy@example", 23)).toBeNull();
  });

  test("rule 3: no whitespace between the trigger and the cursor", () => {
    expect(detectTrigger("hey @matt is here", 17)).toBeNull();
  });

  test("offsets are grapheme positions, not JS string indices", () => {
    // §3.3 is explicit. A code-unit index puts the insertion range one position
    // off the moment anyone types an emoji before a mention — and the resulting
    // message has a mangled word in it, not an obvious error.
    const text = "💯 @ma";
    const trigger = detectTrigger(text, 5);
    expect(trigger?.at).toBe(2);
    expect(trigger?.query).toBe("ma");
  });

  test("`@@` is the agents-only trigger", () => {
    const trigger = detectTrigger("ping @@cl", 9);
    expect(trigger?.agentsOnly).toBe(true);
    expect(trigger?.query).toBe("cl");
  });

  test("`/` only triggers at column 0 (§1.4)", () => {
    expect(detectTrigger("/agents", 7)?.kind).toBe("slash");
    // Anywhere else a `/` is a path separator or part of a URL.
    expect(detectTrigger("see crates/buzz-db/src", 22)).toBeNull();
  });
});

describe("§3.3 ranking is deterministic", () => {
  const candidates: MentionCandidate[] = [
    {
      pubkey: "pk_c1",
      handle: "claude-1",
      displayName: "acp",
      isAgent: true,
      presence: "present",
      frecency: 1,
    },
    {
      pubkey: "pk_c2",
      handle: "claude-2",
      displayName: "acp",
      isAgent: true,
      presence: "present",
      frecency: 1,
    },
    {
      pubkey: "pk_c3",
      handle: "claude-3",
      displayName: "acp",
      isAgent: true,
      presence: "present",
      frecency: 1,
    },
  ];

  test("prefix-colliding agent names order identically every call", () => {
    // §3.3: "Prefix-colliding agent names (`claude-1`, `claude-2`, `claude-3` —
    // the orchestrator's normal case) make it worst exactly where it is used
    // most." Equal frecency must not leave the order to insertion chance.
    const a = rankCandidates(candidates, "cl", false).map((c) => c.handle);
    const b = rankCandidates([...candidates].reverse(), "cl", false).map(
      (c) => c.handle,
    );
    expect(a).toEqual(b);
    expect(a).toEqual(["claude-1", "claude-2", "claude-3"]);
  });

  test("an exact prefix match doubles the score", () => {
    const mixed: MentionCandidate[] = [
      {
        pubkey: "pk_a",
        handle: "unclear",
        displayName: "x",
        isAgent: false,
        presence: "present",
        frecency: 2,
      },
      {
        pubkey: "pk_b",
        handle: "clear",
        displayName: "x",
        isAgent: false,
        presence: "present",
        frecency: 2,
      },
    ];
    expect(rankCandidates(mixed, "cle", false)[0]?.handle).toBe("clear");
  });

  test("`@@` filters humans out entirely", () => {
    const mixed: MentionCandidate[] = [
      ...candidates,
      {
        pubkey: "pk_h",
        handle: "claudia",
        displayName: "Claudia",
        isAgent: false,
        presence: "present",
        frecency: 9,
      },
    ];
    const agentsOnly = rankCandidates(mixed, "cla", true);
    expect(agentsOnly.every((c) => c.isAgent)).toBe(true);
  });
});

describe("§3.3 insertion — delete-range-then-insert (P11g)", () => {
  test("a trailing space is added only when one is not already there", () => {
    const first = applyCompletion(
      "hey @ma",
      { kind: "mention", at: 4, query: "ma", agentsOnly: false },
      "@matt",
    );
    expect(first.text).toBe("hey @matt ");

    const second = applyCompletion(
      "hey @ma there",
      { kind: "mention", at: 4, query: "ma", agentsOnly: false },
      "@matt",
    );
    // A second space would leave `@matt  there`, which the user then has to
    // delete — a completion that costs a keystroke to accept.
    expect(second.text).toBe("hey @matt there");
  });

  test("the cursor lands after the inserted text", () => {
    const { text, cursor } = applyCompletion(
      "hey @ma",
      { kind: "mention", at: 4, query: "ma", agentsOnly: false },
      "@matt",
    );
    expect(cursor).toBe(text.length);
  });
});

describe("§3.3 zero candidates", () => {
  test("the picker renders a `no matches` footer rather than nothing", () => {
    // "Enter with zero candidates sends nothing and inserts nothing. It is a
    // no-op that keeps the popup open with a `no matches` footer." Leaving this
    // undefined risks a half-composed message sent by a reflexive Enter.
    const rows = renderMentionPicker([], 0, 60);
    expect(rows.join("\n")).toContain("no matches");
    expect(rows.join("\n")).toContain("esc close");
  });
});

describe("[D-2] MENTION_CAP is surfaced before the send", () => {
  test("the cap is 50 and is reached, not exceeded", () => {
    // "failing at Enter on a message the operator has already written is the
    // worst possible place to learn about a cap."
    expect(MENTION_CAP).toBe(50);
    expect(mentionCapReached(49)).toBe(false);
    expect(mentionCapReached(50)).toBe(true);
  });
});

describe("§2.5 the band is a different surface class from the drawer", () => {
  const composing = (text: string): Session => {
    const s = Session.open("seeded-basic", 100, 26)
      .goTo("Channels")
      .key("right")
      .key("right");
    return s.type(text);
  };

  test("it renders above the top rule, leaving composer and statusline live", () => {
    const s = composing("hey @c");
    const rows = s.screen();
    const rule = rows.findIndex((r) => r.includes("home › channels"));
    const picker = rows.findIndex((r) => r.includes("@claude-1"));
    const composer = rows.findIndex((r) => r.startsWith("❯"));
    expect(picker).toBeGreaterThan(rule);
    expect(composer).toBeGreaterThan(picker);
    // "leaving the composer and statusline fully intact and live" — the drawer
    // replaces them; this class must not.
    expect(s.text()).toContain("buzz://relay.example");
  });

  test("typing narrows it and deleting the trigger closes it", () => {
    const s = composing("hey @cl");
    expect(s.state.completion?.query).toBe("cl");
    expect(s.state.surfaces.has("completion")).toBe(true);
  });

  test("Esc dismisses to literal text — the typed characters stay", () => {
    const s = composing("hey @cl");
    s.key("escape");
    expect(s.state.completion).toBeNull();
    // "esc dismisses to literal text." Deleting what was typed would lose
    // keystrokes, which [G5] forbids everywhere in this design.
    expect(s.state.composer).toBe("hey @cl");
  });

  test("Esc closes the band before anything else — tier 1 (§5.3)", () => {
    const s = composing("hey @cl");
    s.key("escape");
    // The channel must not have been marked read on the way past.
    expect(s.state.pending).toBeNull();
  });

  test("⏎ accepts the selection and records the resolved pubkey [D-2]", () => {
    const s = composing("hey @c");
    s.key("return");
    expect(s.state.composer).toBe("hey @claude-1 ");
    // "what you picked is what gets tagged" — by construction.
    expect(s.state.mentions).toHaveLength(1);
    expect(s.state.mentions[0]).toContain("pk_cl1");
  });

  test("↑/↓ move the picker's selection, not the timeline (§5.2 row 1)", () => {
    const s = composing("hey @c");
    expect(s.state.completion?.selection).toBe(0);
    s.key("down");
    expect(s.state.completion?.selection).toBe(1);
    // The completion band takes the arrows while it is open, so neither the
    // drawer nor message-select opened underneath it.
    expect(s.state.drawer).toBeNull();
    expect(s.state.messageSelect).toBeNull();
  });

  test("the picked mention rides the draft across a navigation", () => {
    const s = composing("hey @c");
    s.key("return");
    expect(s.state.mentions).toHaveLength(1);
    s.key("g", { ctrl: true });
    expect(s.state.mentions).toHaveLength(0);
    // Restoring `hey @claude-1` with an empty mention list would send a message
    // whose visible mention tags nobody.
    s.goTo("Channels").key("right").key("right");
    expect(s.state.composer).toBe("hey @claude-1 ");
    expect(s.state.mentions).toHaveLength(1);
  });

  test("the send carries the resolved pubkeys", () => {
    const s = composing("hey @c");
    s.key("return").type("ping").key("return");
    // The *drained* effect, not `state.pending`: the shell clears the slot as
    // it hands the effect over, and [D-2]'s guarantee is about what reaches
    // the daemon rather than about what the reducer briefly held.
    const effect = s.effects.at(-1);
    expect(effect).toMatchObject({
      kind: "send",
      channelId: "ch_engineering",
      content: "hey @claude-1 ping",
    });
    if (effect?.kind !== "send") throw new Error("expected a send");
    expect(effect.mentions).toHaveLength(1);
  });
});
