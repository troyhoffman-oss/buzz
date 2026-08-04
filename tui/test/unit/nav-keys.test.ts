/**
 * T0 units for the key priority chains — NAVIGATION.md §5.1, §5.2, §5.3.
 *
 * §5's chains are written as ordered lists, so these tests are written as the
 * same lists: one case per row, plus the cases that prove a row does **not**
 * fire when a higher one applies. A chain tested only on its happy path is a
 * chain whose ordering is untested, and the ordering is the entire design.
 */

import { describe, expect, test } from "bun:test";
import {
  type ComposerState,
  type KeyContext,
  clampSelection,
  resolveEnter,
  resolveKey,
  resolveLeft,
  resolveRight,
  resolveShiftVertical,
  resolveVertical,
} from "../../src/nav/keys";
import { type SurfaceSet, resolveEscape } from "../../src/nav/surfaces";

const emptyComposer: ComposerState = {
  text: "",
  cursor: 0,
  multiline: false,
  cursorRow: 0,
  rowCount: 1,
};

const ctx = (over: Partial<KeyContext> = {}): KeyContext => ({
  layer: "channel",
  composer: emptyComposer,
  surfaces: new Set() as SurfaceSet,
  drawerOpen: false,
  drawerExpanded: false,
  messageSelect: false,
  groupHeaderSelected: false,
  attentionRowSelected: false,
  ...over,
});

const withText = (text: string): ComposerState => ({
  ...emptyComposer,
  text,
  cursor: text.length,
});

describe("§5.1 — the ⏎ chain, in order", () => {
  test("1. completion band open → accept", () => {
    expect(
      resolveEnter(
        ctx({
          surfaces: new Set(["completion"]),
          composer: withText("hey @ma"),
        }),
      ),
    ).toEqual({ kind: "completionAccept" });
  });

  test("2. composer has text → send, even at a layer that would descend", () => {
    // §4.1's last step: "The composer had text, so `⏎` sends — the descend
    // interpretation never fires."
    expect(resolveEnter(ctx({ composer: withText("ack") }))).toEqual({
      kind: "send",
    });
  });

  test("2 beats 4: text wins over an open drawer", () => {
    expect(
      resolveEnter(ctx({ composer: withText("ack"), drawerOpen: true })),
    ).toEqual({ kind: "send" });
  });

  test("3. group header selected → collapse / expand", () => {
    expect(
      resolveEnter(ctx({ layer: "home", groupHeaderSelected: true })),
    ).toEqual({
      kind: "toggleGroup",
    });
  });

  test("4. drawer open, row selected → PEEK", () => {
    expect(resolveEnter(ctx({ drawerOpen: true }))).toEqual({ kind: "peek" });
  });

  test("5. L0 attention row selected → PEEK", () => {
    // §4.4: "`⏎` peeks the item in place at the bottom — enough to triage
    // without leaving home."
    expect(
      resolveEnter(ctx({ layer: "home", attentionRowSelected: true })),
    ).toEqual({
      kind: "peek",
    });
  });

  test("6. message-select → open thread (no peek presentation)", () => {
    expect(resolveEnter(ctx({ messageSelect: true }))).toEqual({
      kind: "openThread",
    });
  });

  test("7. otherwise → descend, identical to →", () => {
    expect(resolveEnter(ctx({ layer: "channels" }))).toEqual({
      kind: "descend",
    });
  });

  test("⏎ is never a no-op: every context resolves to an action", () => {
    // §5.1: "so `⏎` is never wrong — it is either the cheaper of two moves or
    // the only one."
    const contexts = [
      ctx(),
      ctx({ layer: "home" }),
      ctx({ layer: "channels" }),
      ctx({ drawerOpen: true }),
      ctx({ messageSelect: true }),
      ctx({ composer: withText("x") }),
    ];
    for (const c of contexts) expect(resolveEnter(c).kind).not.toBe("none");
  });
});

describe("§5.2 — the ↑/↓ chain, in order", () => {
  test("completion band open → move selection", () => {
    expect(
      resolveVertical(ctx({ surfaces: new Set(["completion"]) }), -1),
    ).toEqual({
      kind: "completionMove",
      delta: -1,
    });
  });

  test("multiline composer with a row to move to → move the text cursor", () => {
    const composer: ComposerState = {
      text: "one\ntwo",
      cursor: 5,
      multiline: true,
      cursorRow: 1,
      rowCount: 2,
    };
    expect(resolveVertical(ctx({ composer }), -1)).toEqual({
      kind: "moveTextCursor",
      delta: -1,
    });
  });

  test("multiline composer with NO row to move to falls through", () => {
    // Without the "has a row to move to" qualifier, `↑` on the first row would
    // swallow the press and the user would learn that `↑` sometimes does
    // nothing.
    const composer: ComposerState = {
      text: "one\ntwo",
      cursor: 1,
      multiline: true,
      cursorRow: 0,
      rowCount: 2,
    };
    expect(resolveVertical(ctx({ composer }), -1)).toEqual({
      kind: "moveTextEdge",
      edge: "start",
    });
  });

  test("single-line composer with text → move to start / end of input", () => {
    expect(resolveVertical(ctx({ composer: withText("hey") }), -1)).toEqual({
      kind: "moveTextEdge",
      edge: "start",
    });
    expect(resolveVertical(ctx({ composer: withText("hey") }), 1)).toEqual({
      kind: "moveTextEdge",
      edge: "end",
    });
  });

  test("drawer or message-select active → move selection", () => {
    expect(resolveVertical(ctx({ drawerOpen: true }), 1)).toEqual({
      kind: "moveSelection",
      delta: 1,
    });
    expect(resolveVertical(ctx({ messageSelect: true }), -1)).toEqual({
      kind: "moveSelection",
      delta: -1,
    });
  });

  test("in an expanded drawer the arrows scroll the tail, not the list (§2.4)", () => {
    expect(resolveVertical(ctx({ drawerExpanded: true }), 1)).toEqual({
      kind: "scrollTail",
      delta: 1,
    });
  });

  test("PICKER LAYER, empty composer → move the list selection", () => {
    for (const layer of ["home", "channels", "agents", "results"] as const) {
      expect(resolveVertical(ctx({ layer }), 1)).toEqual({
        kind: "moveSelection",
        delta: 1,
      });
      expect(resolveVertical(ctx({ layer }), -1)).toEqual({
        kind: "moveSelection",
        delta: -1,
      });
    }
  });

  test("CHAT LAYER, empty composer → ↑ enters message-select, ↓ opens the drawer", () => {
    // The two level-1 surfaces of §3: "`↑` = go up the conversation. `↓` = go
    // down into what is running."
    for (const layer of ["channel", "thread", "activity"] as const) {
      expect(resolveVertical(ctx({ layer }), -1)).toEqual({
        kind: "enterMessageSelect",
      });
      expect(resolveVertical(ctx({ layer }), 1)).toEqual({
        kind: "openDrawer",
      });
    }
  });

  test("neither surface exists on a picker layer (§2.3's one rule)", () => {
    // "On picker layers the list already holds `❯` and the arrows move it — and
    // there is nothing to peek at, because a picker layer's body *is* the list
    // of live things the drawer would have shown."
    for (const layer of ["home", "channels", "agents"] as const) {
      expect(resolveVertical(ctx({ layer }), 1).kind).not.toBe("openDrawer");
      expect(resolveVertical(ctx({ layer }), -1).kind).not.toBe(
        "enterMessageSelect",
      );
    }
  });
});

describe("§1.1 — ⇧↑/⇧↓ jump by structural unit", () => {
  test("it is one gesture with one meaning at every layer", () => {
    expect(resolveShiftVertical(ctx({ layer: "home" }), 1)).toEqual({
      kind: "jumpStructural",
      delta: 1,
    });
    expect(resolveShiftVertical(ctx({ messageSelect: true }), -1)).toEqual({
      kind: "jumpStructural",
      delta: -1,
    });
  });

  test("it does not fire while the composer holds text", () => {
    expect(resolveShiftVertical(ctx({ composer: withText("x") }), 1).kind).toBe(
      "none",
    );
  });
});

describe("§1.1 — → commits and ← ascends", () => {
  test("→ descends when there is no text to its right", () => {
    expect(resolveRight(ctx({ layer: "channels" }))).toEqual({
      kind: "descend",
    });
  });

  test("→ moves the text cursor while there is text to its right", () => {
    const composer: ComposerState = {
      ...emptyComposer,
      text: "hey",
      cursor: 1,
    };
    expect(resolveRight(ctx({ composer }))).toEqual({
      kind: "moveTextCursor",
      delta: 1,
    });
  });

  test("← ascends, and never dismisses", () => {
    expect(resolveLeft(ctx({ layer: "channel" }))).toEqual({ kind: "ascend" });
  });

  test("← inside the drawer pops the expansion and is a no-op at list level", () => {
    // §2.4's table: "pop expansion → list. At list level, **no-op** [G3]."
    // Closing the drawer is `Esc`'s job — keeping the two verbs disjoint is
    // what lets a user hold `←` without wondering what they dismissed.
    expect(resolveLeft(ctx({ drawerExpanded: true })).kind).toBe("peek");
    expect(resolveLeft(ctx({ drawerOpen: true })).kind).toBe("none");
  });
});

describe("[G5] — printable keys are never lost", () => {
  test("a printable at drawer list level closes and inserts in one keystroke", () => {
    // §2.4: "the single most important behavior to port."
    expect(
      resolveKey(ctx({ drawerOpen: true }), { name: "o", char: "o" }),
    ).toEqual({
      kind: "insertChar",
      char: "o",
    });
  });

  test("a printable during message-select dismisses and inserts", () => {
    expect(
      resolveKey(ctx({ messageSelect: true }), { name: "a", char: "a" }),
    ).toEqual({
      kind: "insertChar",
      char: "a",
    });
  });

  test("a printable in an expanded drawer is captured, not inserted", () => {
    // §2.4's table: "At expanded level: captured." You deliberately descended.
    expect(
      resolveKey(ctx({ drawerExpanded: true }), { name: "o", char: "o" }).kind,
    ).toBe("none");
  });

  test("a printable at a picker layer types into the composer", () => {
    // Which is what moves `❯` from the list to the composer (§2.2) and what
    // §8 ruling 1 makes the channel-list filter.
    expect(
      resolveKey(ctx({ layer: "channels" }), { name: "e", char: "e" }),
    ).toEqual({
      kind: "insertChar",
      char: "e",
    });
  });
});

describe("§1.4 — the direct shortcuts are all chorded", () => {
  test("ctrl+k, ctrl+p, ctrl+g, ctrl+f resolve", () => {
    expect(resolveKey(ctx(), { name: "k", ctrl: true }).kind).toBe(
      "openSearch",
    );
    expect(resolveKey(ctx(), { name: "p", ctrl: true }).kind).toBe(
      "openPalette",
    );
    expect(resolveKey(ctx(), { name: "g", ctrl: true }).kind).toBe("goHome");
    expect(resolveKey(ctx(), { name: "f", ctrl: true }).kind).toBe(
      "findInChannel",
    );
  });

  test("ctrl+↑ / ctrl+↓ are previous / next unread, not selection moves", () => {
    expect(resolveKey(ctx(), { name: "up", ctrl: true })).toEqual({
      kind: "unreadJump",
      delta: -1,
    });
    expect(resolveKey(ctx(), { name: "down", ctrl: true })).toEqual({
      kind: "unreadJump",
      delta: 1,
    });
  });

  test("a shortcut fires even while the composer holds text", () => {
    // A chord cannot be a composer keystroke, so it has no more local
    // interpretation to lose to.
    expect(
      resolveKey(ctx({ composer: withText("draft") }), {
        name: "g",
        ctrl: true,
      }).kind,
    ).toBe("goHome");
  });

  test("no bare letter is bound [G5]", () => {
    // §1.4: "no bare letter is bound anywhere outside a modal detail view,
    // because bare letters must stay available to the composer."
    for (const letter of "abcdefghijklmnopqrstuvwxyz") {
      expect(resolveKey(ctx(), { name: letter, char: letter })).toEqual({
        kind: "insertChar",
        char: letter,
      });
    }
  });

  test("Tab and hjkl are deliberately unbound (§5.2's negative space)", () => {
    expect(resolveKey(ctx(), { name: "tab" }).kind).toBe("none");
    // `hjkl` are printable and therefore type — they are not navigation.
    expect(resolveKey(ctx(), { name: "j", char: "j" }).kind).toBe("insertChar");
  });
});

describe("§5.3 — Esc's three tiers", () => {
  test("tier 1: an innermost control consumes it", () => {
    expect(resolveEscape(new Set(["completion"]))).toEqual({
      tier: 1,
      consumedBy: "completion",
    });
  });

  test("tier 2 closes in the specified order, not in registration order", () => {
    // "Tier 2 is a counter, not z-order, and not a naive focus-stack pop."
    // Registering the drawer *after* its expansion must still close the
    // expansion first.
    expect(resolveEscape(new Set(["drawer", "drawerExpansion"]))).toEqual({
      tier: 2,
      close: "drawerExpansion",
    });
    expect(resolveEscape(new Set(["messageSelect", "modal"]))).toEqual({
      tier: 2,
      close: "modal",
    });
  });

  test("tier 3 is mark-channel-read, and only with nothing registered", () => {
    expect(resolveEscape(new Set())).toEqual({
      tier: 3,
      action: "markChannelRead",
    });
    // The suppression is what makes `Esc` safe to press repeatedly: walking out
    // of a drawer expansion takes two presses and neither silently marks read.
    expect(resolveEscape(new Set(["drawer"])).tier).toBe(2);
  });
});

describe("[G4] — clamped, no wrap", () => {
  test("boundaries are never trapdoors", () => {
    // A wrap at the top of ATTENTION would drop you at the bottom of PLACES,
    // which reads as a teleport you did not ask for.
    expect(clampSelection(-1, 5)).toBe(0);
    expect(clampSelection(5, 5)).toBe(4);
    expect(clampSelection(2, 5)).toBe(2);
    expect(clampSelection(0, 0)).toBe(0);
  });
});
