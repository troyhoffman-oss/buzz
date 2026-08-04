/**
 * T0 units for the layer spine — NAVIGATION.md §1, §2.2.
 */

import { describe, expect, test } from "bun:test";
import {
  breadcrumb,
  composerPlaceholder,
  current,
  depth,
  goHome,
  isChatLayer,
  isPickerLayer,
  newStack,
  pop,
  push,
  sendTarget,
  setSelection,
} from "../../src/nav/layers";

const channels = { kind: "channels" as const, crumb: "channels", selection: 0 };
const engineering = {
  kind: "channel" as const,
  channelId: "ch_engineering",
  crumb: "#engineering",
  selection: 0,
};

describe("depth is the column index of §1's map", () => {
  test("a fresh stack is at L0", () => {
    const stack = newStack();
    expect(depth(stack)).toBe(0);
    expect(current(stack).kind).toBe("home");
  });

  test("each push descends one column", () => {
    const stack = push(push(newStack(), channels), engineering);
    expect(depth(stack)).toBe(2);
    expect(current(stack).kind).toBe("channel");
  });
});

describe("← is a pure depth verb (§1.1, [G3])", () => {
  test("at L0 it is a no-op — never a quit, never a wrap", () => {
    // §5.4 gates `←` at "none — instant" precisely because it cannot do damage.
    // A `←` that quit the app would make holding it dangerous.
    const stack = newStack();
    expect(pop(stack)).toEqual(stack);
    expect(depth(pop(stack))).toBe(0);
  });

  test("it restores the selection you left from", () => {
    // §1.1: "not decoration — it is the whole reason the spine reads as
    // spatial. Descend and ascend are inverses; the layer you return to is the
    // layer you left, not its first row."
    let stack = push(newStack(), channels);
    stack = setSelection(stack, 4);
    stack = push(stack, engineering);
    expect(current(pop(stack)).selection).toBe(4);
  });
});

describe("ctrl+g collapses to L0 (§1.4)", () => {
  test("home's own selection survives the collapse", () => {
    let stack = setSelection(newStack(), 3);
    stack = push(push(stack, channels), engineering);
    const home = goHome(stack);
    expect(depth(home)).toBe(0);
    expect(current(home).selection).toBe(3);
  });
});

describe("focus ownership — 'whoever owns the ❯ owns the arrows' (§2.2)", () => {
  test("L2, L3 and L4 are chat layers; the composer owns ❯ there", () => {
    expect(isChatLayer("channel")).toBe(true);
    expect(isChatLayer("thread")).toBe(true);
    expect(isChatLayer("activity")).toBe(true);
  });

  test("L0 and L1 are picker layers; the list owns ❯ there", () => {
    // §2.3 derives the drawer and message-select from exactly this predicate:
    // both exist only where the arrows would otherwise be idle.
    expect(isPickerLayer("home")).toBe(true);
    expect(isPickerLayer("channels")).toBe(true);
    expect(isPickerLayer("agents")).toBe(true);
    expect(isPickerLayer("results")).toBe(true);
  });
});

describe("composer residency per layer (§2.2, §8 ruling 1)", () => {
  test("L1 CHANNELS is jump/filter only — no post-in-place", () => {
    // §8 ruling 1 resolved the open question against posting to the highlighted
    // channel without entering it. The placeholder must say so, or the
    // capability is advertised and absent.
    expect(composerPlaceholder(channels)).toBe("filter channels");
    expect(sendTarget(channels)).toBeNull();
  });

  test("L2 sends to the channel — the 95% case", () => {
    expect(sendTarget(engineering)).toEqual({
      kind: "channel",
      channelId: "ch_engineering",
    });
    expect(composerPlaceholder(engineering)).toBe("message #engineering");
  });

  test("L3 replies to the thread root", () => {
    const thread = {
      kind: "thread" as const,
      channelId: "ch_engineering",
      eventId: "ev_eng_002",
      crumb: "⤷ read-state slots",
      selection: 0,
    };
    expect(composerPlaceholder(thread)).toBe("reply in thread");
    expect(sendTarget(thread)).toEqual({
      kind: "thread",
      rootEventId: "ev_eng_002",
      channelId: "ch_engineering",
    });
  });

  test("L4 steers the agent turn", () => {
    const activity = {
      kind: "activity" as const,
      agentPubkey: "claude-1",
      crumb: "claude-1",
      selection: 0,
    };
    expect(composerPlaceholder(activity)).toBe("steer claude-1");
    expect(sendTarget(activity)).toEqual({ kind: "agent", pubkey: "claude-1" });
  });

  test("home and results have no send target", () => {
    expect(sendTarget(current(newStack()))).toBeNull();
    expect(
      sendTarget({
        kind: "results",
        crumb: "results",
        query: "x",
        selection: 0,
      }),
    ).toBeNull();
  });
});

describe("the breadcrumb records the path taken, not the tree position ([G15])", () => {
  test("a teleport reads through the group it came from", () => {
    // §4.4: "`←` returns to the mentions list, not to a channel list you never
    // visited." No function of the destination alone can produce this crumb.
    const stack = push(
      push(newStack(), { kind: "channels", crumb: "mentions", selection: 0 }),
      engineering,
    );
    expect(breadcrumb(stack)).toBe("home › mentions › #engineering");
  });

  test("the walked path reads through channels", () => {
    const stack = push(push(newStack(), channels), engineering);
    expect(breadcrumb(stack)).toBe("home › channels › #engineering");
  });
});
