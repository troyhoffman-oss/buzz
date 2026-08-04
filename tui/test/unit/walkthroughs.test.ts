/**
 * NAVIGATION.md §4's four walkthroughs, driven as key sequences.
 *
 * These are the spec's own acceptance criteria. Each test's name is the
 * walkthrough's heading and each body is its keystroke line, so a divergence
 * between the document and the app shows up as a failing test rather than as a
 * rendering that "looks about right".
 */

import { describe, expect, test } from "bun:test";
import { Session } from "../helpers/drive";

describe("§4.1 — enter a channel and reply", () => {
  // ctrl+g → ⇧↓ ↓ → → → type → ⏎
  test("home opens on the top mention (§1.1 default selection)", () => {
    const s = Session.open("seeded-basic");
    // "Home therefore opens on your top mention when you have one." Without
    // this, §4.4's two-keystroke jump becomes a five-keystroke one.
    expect(s.focusRow()).toContain("matt");
    expect(s.focusRow()).toContain("#engineering");
  });

  test("⇧↓ jumps to the next group header; ↓ steps onto its first row", () => {
    const s = Session.open("seeded-basic");
    // §4.1: "`⇧↓` jumps to the next group header (PLACES); `↓` steps onto its
    // first row." Each `⇧↓` is **one** structural boundary — the count needed
    // to reach PLACES is a property of the fixture's group list, so the test
    // walks until it arrives rather than hardcoding a number.
    let guard = 0;
    while (!s.focusRow()?.includes("PLACES") && guard++ < 20) {
      s.key("down", { shift: true });
    }
    expect(s.focusRow()).toContain("PLACES");
    s.key("down");
    expect(s.focusRow()).toContain("Channels");
  });

  test("→ from PLACES ▸ Channels descends to L1", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right");
    expect(s.layer()).toBe("channels");
    expect(s.crumb()).toBe("home › channels");
  });

  test("→ from L1 descends to L2, and the composer retargets", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right").key("right");
    expect(s.layer()).toBe("channel");
    expect(s.crumb()).toBe("home › channels › #engineering");
    // §2.2's placeholder table: L2 CHANNEL sends to the channel.
    expect(s.text()).toContain("message #engineering");
  });

  test("with text in the composer, ⏎ sends — descend never fires (§5.1 row 2)", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right").key("right");
    s.type("ack");
    expect(s.state.composer).toBe("ack");
    s.key("return");
    // The composer clears and an effect is *delivered* for the channel, not a
    // descent into a thread.
    //
    // Asserted on the drained effect rather than on `state.pending`: `pending`
    // is the reducer's intermediate, and the shell now clears it as it hands
    // the effect over. Asserting on the intermediate would pass whether or not
    // anyone drains it — which is how the missing drain went unnoticed.
    expect(s.state.composer).toBe("");
    expect(s.state.pending).toBeNull();
    expect(s.effects).toEqual([
      {
        kind: "send",
        channelId: "ch_engineering",
        content: "ack",
        // [D-2]: mentions are resolved pubkeys accumulated at pick time. An
        // unmentioned message carries an empty list, never an absent field —
        // the daemon's `extract_at_mentions_with_known` fallback exists for
        // `curl` and second clients, and the TUI must not use it.
        mentions: [],
      },
    ]);
    expect(s.layer()).toBe("channel");
  });
});

describe("§4.2 — pick a thread in a busy channel", () => {
  // From L2 with an empty composer: ↑ → ⇧↑ ⇧↑ → →
  const atChannel = (): Session => {
    const s = Session.open("seeded-basic");
    return s.goTo("Channels").key("right").key("right");
  };

  test("↑ enters message-select at the newest message; ❯ moves into the timeline", () => {
    const s = atChannel();
    expect(s.state.messageSelect).toBeNull();
    s.key("up");
    expect(s.state.messageSelect).not.toBeNull();
    // §4.2: "The composer is gone; the `❯` has moved into the timeline [G8]."
    expect(s.focusGlyphCount()).toBe(1);
    expect(s.focusRow()).toContain("ack — I'll fix the observer to match");
    expect(s.text()).toContain("↑/↓ message");
  });

  test("⇧↑ steps thread root to thread root, skipping reply-less messages", () => {
    const s = atChannel().key("up");
    // The newest message (troy's `ack`) has no replies. One `⇧↑` must land on
    // matt's 14:09, which does — skipping nothing in between here, but the
    // second must skip the diff and claude-1's reply to reach troy's 13:41.
    s.key("up", { shift: true });
    expect(s.focusRow()).toContain("44200 cadence");
    s.key("up", { shift: true });
    expect(s.focusRow()).toContain("read-state slots cap at 8");
  });

  test("the counts survive truncation at every width (§3, [G10])", () => {
    for (const cols of [120, 80, 60]) {
      const s = Session.open("seeded-basic", cols, 30);
      s.goTo("Channels")
        .key("right")
        .key("right")
        .key("up")
        .key("up", { shift: true });
      // "the label ellipsizes, the counts do not" — this is the row you are
      // picking a thread *by*.
      expect(s.text()).toContain("⤷ 4 · 2 new");
    }
  });

  test("⇧↑ clamps at the oldest root rather than wrapping [G4]", () => {
    const s = atChannel().key("up");
    for (let i = 0; i < 10; i++) s.key("up", { shift: true });
    const settled = s.state.messageSelect?.index;
    s.key("up", { shift: true });
    // "boundaries are never trapdoors" — a wrap here would teleport the cursor
    // to the newest message, which reads as a jump you did not ask for.
    expect(s.state.messageSelect?.index).toBe(settled ?? -1);
  });

  test("→ opens the thread at L3 with the thread composer resident", () => {
    const s = atChannel()
      .key("up")
      .key("up", { shift: true })
      .key("up", { shift: true });
    s.key("right");
    expect(s.layer()).toBe("thread");
    expect(s.crumb()).toContain("home › channels › #engineering › ⤷");
    // §2.2: L3 THREAD replies to the thread root.
    expect(s.text()).toContain("reply in thread");
  });

  test("← returns to L2 with the same message still selected (§1.1)", () => {
    const s = atChannel().key("up").key("up", { shift: true });
    const selected = s.state.messageSelect?.index;
    s.key("right");
    expect(s.layer()).toBe("thread");
    s.key("left");
    expect(s.layer()).toBe("channel");
    // §4.2: "`←` returns to L2 **with the same message still selected**." The
    // ascent restores the layer you left, not its first row.
    expect(s.state.stack.entries.at(-1)?.selection).toBe(selected ?? -1);
  });
});

describe("§4.3 — peek agent activity while chatting", () => {
  // From L2, composer empty: ↓ → ⏎ → read → Esc → keep typing
  const atChannel = (): Session => {
    const s = Session.open("seeded-basic");
    return s.goTo("Channels").key("right").key("right");
  };

  test("↓ replaces composer + statusline with the drawer; chat is compressed", () => {
    const s = atChannel();
    const chatBefore = s.text();
    expect(chatBefore).toContain("44200 cadence");
    s.key("down");
    const withDrawer = s.text();
    // §2.3: it replaces the bands and grows upward. The chat is still there —
    // "compressed, never evicted" [G16].
    expect(withDrawer).toContain("Live");
    expect(withDrawer).toContain("AGENTS");
    expect(withDrawer).toContain("44200 cadence");
    // The statusline is gone while the drawer is up.
    expect(withDrawer).not.toContain("buzz://relay.example");
  });

  test("rows are in [G12] ladder order: needs-input before working", () => {
    const s = atChannel().key("down");
    const rows = s.screen();
    const needsInput = rows.findIndex((r) => r.includes("needs input"));
    const working = rows.findIndex(
      (r) => r.includes("working") && r.includes("claude-1"),
    );
    expect(needsInput).toBeGreaterThanOrEqual(0);
    expect(working).toBeGreaterThanOrEqual(0);
    expect(needsInput).toBeLessThan(working);
  });

  test("⏎ expands in place — the chat is compressed further, never replaced", () => {
    const s = atChannel().key("down").key("return");
    expect(s.state.drawer?.expanded).toBe(true);
    expect(s.text()).toContain("Transcript");
    expect(s.text()).toContain("Agent");
  });

  test("Esc collapses to the list; Esc again restores the composer", () => {
    const s = atChannel().key("down").key("return");
    s.key("escape");
    expect(s.state.drawer?.expanded).toBe(false);
    expect(s.state.drawer).not.toBeNull();
    s.key("escape");
    expect(s.state.drawer).toBeNull();
    expect(s.text()).toContain("message #engineering");
  });

  test("typing 'o' at list level closes the drawer AND inserts, in one keystroke", () => {
    // §2.4: "the single most important behavior to port." Two keystrokes would
    // make `↓` a gesture you have to undo before typing.
    const s = atChannel().key("down");
    s.press({ name: "o", char: "o" });
    expect(s.state.drawer).toBeNull();
    expect(s.state.composer).toBe("o");
    expect(s.text()).toContain("❯ o");
  });

  test("a printable in the EXPANDED drawer is captured, not inserted", () => {
    const s = atChannel().key("down").key("return");
    s.press({ name: "o", char: "o" });
    expect(s.state.drawer?.expanded).toBe(true);
    expect(s.state.composer).toBe("");
  });

  test("→ from the drawer commits to L4 ACTIVITY; ← returns to the channel", () => {
    const s = atChannel().key("down").key("right");
    expect(s.layer()).toBe("activity");
    // §4.3: "`←` from there returns to `#engineering`, because the back stack
    // remembers the path in [G15]."
    s.key("left");
    expect(s.layer()).toBe("channel");
    expect(s.crumb()).toBe("home › channels › #engineering");
  });
});

describe("§4.4 — jump to a mention", () => {
  test("ctrl+g collapses to L0 preserving home's own selection (§1.4)", () => {
    const s = Session.open("seeded-basic");
    // `ctrl+g` is "go home (collapse to L0)" — it restores home *as you left
    // it*, not as it booted. Walking to PLACES ▸ Channels and descending
    // therefore leaves home selecting Channels, and `→` on return re-descends
    // there rather than teleporting to a mention.
    s.goTo("Channels").key("right").key("right");
    expect(s.layer()).toBe("channel");

    s.key("g", { ctrl: true });
    expect(s.layer()).toBe("home");
    expect(s.focusRow()).toContain("Channels");
    s.key("right");
    expect(s.layer()).toBe("channels");
  });

  test("from a fresh boot the mention teleport is two keystrokes", () => {
    // §4.4's actual claim: "Home opens already selecting the top mention
    // (§1.1), so this is two keystrokes." The `ctrl+g` in the walkthrough's
    // key line is the *arrival* at home, not a re-selection of it.
    const s = Session.open("seeded-basic");
    expect(s.focusRow()).toContain("44200 cadence");
    s.key("right");
    expect(s.layer()).toBe("channel");
    expect(s.crumb()).toBe("home › mentions › #engineering");
  });

  test("→ teleports straight to L2, skipping L1 entirely", () => {
    const s = Session.open("seeded-basic").key("right");
    expect(s.layer()).toBe("channel");
    expect(s.state.stack.entries.at(-1)?.channelId).toBe("ch_engineering");
    // The anchor event is carried, so the timeline can position at the message.
    expect(s.state.stack.entries.at(-1)?.eventId).toBe("ev_eng_006");
  });

  test("the breadcrumb records the path taken, not the tree position [G15]", () => {
    const s = Session.open("seeded-basic").key("right");
    // §4.4: "`←` returns to the mentions list, not to a channel list you never
    // visited."
    expect(s.crumb()).toBe("home › mentions › #engineering");
    expect(s.crumb()).not.toContain("channels");
  });

  test("⏎ on an attention row peeks in place rather than travelling (§5.1 row 5)", () => {
    const s = Session.open("seeded-basic");
    s.key("return");
    // "enough to triage without leaving home" — the layer must not change.
    expect(s.layer()).toBe("home");
  });

  test("⏎ on a group header collapses the group (§5.1 row 3)", () => {
    const s = Session.open("seeded-basic");
    s.key("up"); // onto the MENTIONS header
    expect(s.focusRow()).toContain("MENTIONS");
    s.key("return");
    expect(s.state.collapsedGroups.has("mentions")).toBe(true);
    // The group's rows are gone; the header remains, now marked collapsed.
    expect(s.text()).toContain("▸ MENTIONS");
    expect(s.text()).not.toContain("44200 cadence is every turn");
  });
});

describe("cross-cutting invariants", () => {
  test("[G8] there is exactly one ❯ on screen, on every layer", () => {
    const s = Session.open("seeded-basic");
    expect(s.focusGlyphCount()).toBe(1);
    s.goTo("Channels");
    expect(s.focusGlyphCount()).toBe(1);
    s.key("right");
    expect(s.focusGlyphCount()).toBe(1);
    s.key("right");
    expect(s.focusGlyphCount()).toBe(1);
    s.key("up"); // message-select
    expect(s.focusGlyphCount()).toBe(1);
    s.key("escape").key("down"); // drawer
    expect(s.focusGlyphCount()).toBe(1);
  });

  test("§5.2 ← walks the cursor through text before it ascends", () => {
    // Not a bug, and worth pinning because it reads like one: with a draft in
    // the composer, `←` is a *text* key until the cursor reaches column 0, and
    // only the press after that ascends. §5.2's rule is "arrows type whenever
    // there is text under the cursor to move through, and navigate only when
    // that interpretation is vacuous" — so a half-written reply is not
    // something you can accidentally `←` out of mid-word.
    const s = Session.open("seeded-basic", 100, 26)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("up")
      .key("up", { shift: true })
      .key("right");
    expect(s.layer()).toBe("thread");

    s.type("draft");
    expect(s.state.cursor).toBe(5);
    for (let i = 0; i < 5; i++) s.key("left");
    // Five presses spent on the text; still in the thread, draft intact.
    expect(s.state.cursor).toBe(0);
    expect(s.layer()).toBe("thread");
    expect(s.state.composer).toBe("draft");

    s.key("left");
    expect(s.layer()).toBe("channel");
    // §5.4: the draft is persisted per layer, not discarded by the ascent.
    s.key("right");
    expect(s.state.composer).toBe("draft");
  });

  test("[G8] zero ❯ is as broken as two — L4 ACTIVITY has no conversation", () => {
    // Regression. ACTIVITY is a chat layer (the composer owns `❯`, so `↓`
    // opens the drawer) but its body is a *transcript*, not a message list.
    // Because it carries a `channelId` for its back target, "the messages for
    // this layer" resolved to the channel's messages — so `↑` replaced the
    // composer with a hint footer and put the cursor on a message that was
    // nowhere on screen. Nothing was marked, and the glyph's whole job is to
    // say where keys go.
    const s = Session.open("seeded-basic", 100, 26)
      .goTo("Channels")
      .key("right")
      .key("right")
      .key("down")
      .key("down")
      .key("right");
    expect(s.layer()).toBe("activity");
    s.key("up");
    expect(s.state.messageSelect).toBeNull();
    expect(s.focusGlyphCount()).toBe(1);
  });

  test("§2.4 the drawer's ❯ and its → target are the same row", () => {
    // Regression. The reducer indexed the *unfitted* row list while the
    // renderer marked a row in the *fitted* one, so under §2.3's degradation
    // the glyph sat on one row and `→` committed to another. Invisible until
    // the drawer degrades — which is exactly when a user can least tell a
    // mis-navigation from a crowded screen. Matching by row identity is what
    // keeps them in agreement at every height.
    for (const rows of [30, 20, 18]) {
      const s = Session.open("seeded-basic", 100, rows)
        .goTo("Channels")
        .key("right")
        .key("right")
        .key("down");
      s.key("down");
      const marked = s.focusRow() ?? "";
      s.key("right");
      // Whatever was marked is what we arrived at.
      const name =
        marked
          .replace("❯", "")
          .trim()
          .split(/\s{2,}/)[0] ?? "";
      expect(name.length).toBeGreaterThan(0);
      expect(s.crumb()).toContain(name);
    }
  });

  test("[G3] ← at L0 is a no-op, not a quit and not a wrap", () => {
    const s = Session.open("seeded-basic");
    const before = s.text();
    s.key("left").key("left").key("left");
    expect(s.layer()).toBe("home");
    expect(s.text()).toBe(before);
  });

  test("§5.4 drafts are persisted per layer and restored on return", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right").key("right");
    s.type("half a thought");
    s.key("g", { ctrl: true });
    expect(s.state.composer).toBe("");
    // Losing a half-written message to a navigation key is the small betrayal
    // that makes people stop trusting the arrows.
    s.key("right");
    expect(s.state.composer).toBe("");
    s.key("g", { ctrl: true });
    s.goTo("Channels").key("right").key("right");
    expect(s.state.composer).toBe("half a thought");
  });

  test("§2.2 the ❯ moves to the composer on a picker layer once you type", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right");
    expect(s.focusRow()).toContain("#");
    s.type("eng");
    // The row demotes to `▌` and the composer takes `❯` — still exactly one.
    expect(s.focusGlyphCount()).toBe(1);
    expect(s.focusRow()).toContain("eng");
    expect(s.text()).toContain("▌");
  });

  test("§8 ruling 1 — typing at L1 filters; it never posts", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right");
    s.type("buzz");
    expect(s.text()).toContain("#buzz-dev");
    expect(s.text()).not.toContain("#general");
    s.key("return");
    // "Enter enters the highlighted channel. No post-in-place."
    expect(s.layer()).toBe("channel");
    expect(s.state.pending).toBeNull();
    expect(s.effects).toHaveLength(0);
    expect(s.crumb()).toBe("home › channels › #buzz-dev");
  });

  /**
   * The write path is only real if something **drains** the effect.
   *
   * This was a live bug: `dispatch.ts` set `pending` and nothing consumed it,
   * so `⏎` cleared the composer and the message vanished — no error, no log.
   * The clear is the visible half, which is why it looked like it worked.
   *
   * Asserting on `effects` rather than on `pending` is the point: `pending` is
   * an intermediate that a reducer test can see whether or not a caller drains
   * it, so a test written against it would have passed throughout the bug.
   */
  test("⏎ with composer text emits a send effect for the drain to perform", () => {
    const s = Session.open("seeded-basic");
    s.goTo("Channels").key("right").key("right");
    s.type("ship it");
    expect(s.state.composer).toBe("ship it");

    s.key("return");

    expect(s.effects).toHaveLength(1);
    const effect = s.effects[0];
    if (effect?.kind !== "send") throw new Error("expected a send effect");
    expect(effect.content).toBe("ship it");
    expect(effect.channelId).toBeTruthy();
    // The composer clears — but only *alongside* the effect, never instead of
    // it. That pairing is the whole assertion.
    expect(s.state.composer).toBe("");
    expect(s.state.pending).toBeNull();
  });

  /** A reply carries `replyTo`, so a thread post does not land in the channel. */
  test("⏎ in a thread sends with replyTo set to the thread root", () => {
    const s = Session.open("seeded-basic");
    s.key("right").key("up").key("up", { shift: true }).key("right");
    expect(s.layer()).toBe("thread");

    s.type("acked");
    s.key("return");

    const effect = s.effects.at(-1);
    if (effect?.kind !== "send") throw new Error("expected a send effect");
    expect(effect.content).toBe("acked");
    expect(effect.replyTo).toBeTruthy();
  });
});
