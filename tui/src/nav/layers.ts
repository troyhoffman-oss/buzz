/**
 * The layer spine — NAVIGATION.md §1.
 *
 * > Depth == column index. Moving right is descent, moving left is ascent.
 * > Every layer is **full-screen**: one list or one body, the bottom bands
 * > (§2), nothing else. There are no side-by-side panes at any width.
 *
 * The back stack records **the path taken, not the tree position** [G15]. That
 * is the whole reason a teleport works: `←` from a channel entered off a
 * mention returns to the mentions list, not to a channel list you never
 * visited (§4.4).
 */

/** Every layer kind in the Wave-1 spine (§1). */
export type LayerKind =
  /** L0 — ATTENTION zone, PLACES, community row. */
  | "home"
  /** L1 — channels list, fuzzy-filtered. */
  | "channels"
  /** L1 — agents fleet. */
  | "agents"
  /** L1 — search results (`ctrl+k`). */
  | "results"
  /** L2 — channel timeline. The 95% case. */
  | "channel"
  /** L3 — thread body + reply composer. */
  | "thread"
  /** L2 off agents, L4 off a channel — one renderable (§6). */
  | "activity";

/**
 * One entry on the back stack.
 *
 * `selection` is the row the user was on when they left, and restoring it is
 * §1.1's "`←` restoring the selection you left from is not decoration — it is
 * the whole reason the spine reads as spatial."
 */
export interface Layer {
  readonly kind: LayerKind;
  /** Channel id, when the layer is scoped to one. */
  readonly channelId?: string;
  /** Root event id, for `thread`; anchor event id, for a teleported `channel`. */
  readonly eventId?: string;
  /** Agent pubkey, for `activity`. */
  readonly agentPubkey?: string;
  /** Query string, for `results`. */
  readonly query?: string;
  /**
   * Human-readable crumb for §1.2's breadcrumb.
   *
   * Carried on the entry rather than derived from `kind` because the crumb must
   * record the *route* — a channel reached from mentions reads
   * `home › mentions › #engineering`, and no function of the destination alone
   * can produce that.
   */
  readonly crumb: string;
  /** Selected row index within this layer, restored on `←`. */
  selection: number;
}

/**
 * The navigation stack. `entries[0]` is always L0 HOME.
 *
 * Depth is `entries.length - 1`, so it is the literal column index of §1's map.
 */
export interface NavStack {
  readonly entries: Layer[];
}

/** The home layer, which every stack starts from and `ctrl+g` collapses to. */
export function homeLayer(): Layer {
  return { kind: "home", crumb: "home", selection: 0 };
}

/** A fresh stack at L0. */
export function newStack(): NavStack {
  return { entries: [homeLayer()] };
}

/** The layer the user is currently on. Never `undefined` — L0 is always there. */
export function current(stack: NavStack): Layer {
  const top = stack.entries.at(-1);
  if (!top) throw new Error("unreachable: the nav stack always holds L0");
  return top;
}

/** Depth of the current layer: 0 at home, 1 at L1, … (§1's column index). */
export function depth(stack: NavStack): number {
  return stack.entries.length - 1;
}

/**
 * Descend — the `→` verb (§1.1).
 *
 * The only travel verb between layers, unified across every surface. A teleport
 * (§4.4) is this same function called more than once, which is what seeds the
 * back stack with the path taken.
 */
export function push(stack: NavStack, layer: Layer): NavStack {
  return { entries: [...stack.entries, layer] };
}

/**
 * Ascend — the `←` verb (§1.1).
 *
 * > `←` ascend one layer, restoring the selection you left from; never
 * > dismisses; **at L0 it is a no-op** [G3].
 *
 * The no-op is the point: `←` is a pure depth verb, safe to hold. A `←` that
 * quit the app, dismissed a surface, or wrapped around would make holding it
 * dangerous, and §5.4 gates it at "none — instant" precisely because it cannot
 * do damage.
 */
export function pop(stack: NavStack): NavStack {
  if (stack.entries.length <= 1) return stack;
  return { entries: stack.entries.slice(0, -1) };
}

/** `ctrl+g` — collapse to L0, preserving home's own selection (§1.4). */
export function goHome(stack: NavStack): NavStack {
  const first = stack.entries[0];
  if (!first) return newStack();
  return { entries: [first] };
}

/** Record the selected row on the current layer, so `←` can restore it. */
export function setSelection(stack: NavStack, selection: number): NavStack {
  const entries = [...stack.entries];
  const top = entries.at(-1);
  if (!top) return stack;
  entries[entries.length - 1] = { ...top, selection };
  return { entries };
}

/**
 * Layers on which the composer owns `❯` — NAVIGATION.md §2.2.
 *
 * > **Chat layers (L2, L3, L4)** — the composer owns `❯` by default.
 * > **Picker layers (L0, L1)** — the list owns `❯` while the composer is empty.
 *
 * This single predicate is the "whoever owns the `❯` owns the arrows" rule, and
 * §2.3 derives the drawer and message-select from it directly: both exist
 * **only** on chat layers, because that is where the arrows would otherwise be
 * idle. On a picker layer the arrows already move the list, and there is
 * nothing to peek at — the body *is* the list of live things the drawer would
 * have shown. One rule, no exceptions to remember.
 */
export function isChatLayer(kind: LayerKind): boolean {
  return kind === "channel" || kind === "thread" || kind === "activity";
}

/** The complement of {@link isChatLayer}: L0 and L1 (§2.2). */
export function isPickerLayer(kind: LayerKind): boolean {
  return !isChatLayer(kind);
}

/**
 * Composer placeholder per layer — §2.2's table.
 *
 * L1 CHANNELS reads `filter channels`, not `message #engineering`: §8 ruling 1
 * resolved the open question against post-in-place. Typing at the channel list
 * fuzzy-filters and `⏎` enters the highlighted channel; the composer there is
 * jump/filter only.
 *
 * The channel case is the one with history. It used to read
 * `layer.crumb.startsWith("#") ? layer.crumb : layer.channelId`, on the
 * assumption that a channel crumb is `#`-prefixed and anything else is not a
 * name worth showing. **Every fixture bakes the `#` into `name`**
 * (`fixtures/seeded-basic.jsonl` has `"name":"#engineering"`), so that branch
 * always took in test and in every M1/M2 capture — while the relay's own 39000
 * `name` tag is bare, and DMs and forums have no `#` at all. Against live data
 * the fallback therefore always took instead, and the composer read
 * `message 5705545b-3100-4872-93e3-b6c815c6e6ce` while the breadcrumb one row
 * above it correctly read `DM`. The crumb is the channel's name; there is no
 * case where the uuid is the better label.
 */
export function composerPlaceholder(layer: Layer): string {
  switch (layer.kind) {
    case "home":
      return "search or command";
    case "channels":
      return "filter channels";
    case "agents":
      return "filter agents";
    case "results":
      return "search";
    case "channel":
      return `message ${layer.crumb || (layer.channelId ?? "")}`;
    case "thread":
      return "reply in thread";
    case "activity":
      return `steer ${layer.agentPubkey ?? "agent"}`;
  }
}

/**
 * What `⏎` with composer text sends to — §2.2's "`⏎` sends to" column.
 *
 * `null` means the layer has no send target, and per §8 ruling 1 that includes
 * every picker layer: at L1 CHANNELS the text is a filter, so `⏎` enters the
 * highlighted row rather than posting to it.
 */
export type SendTarget =
  | { kind: "channel"; channelId: string }
  | { kind: "thread"; rootEventId: string; channelId: string }
  | { kind: "agent"; pubkey: string }
  | null;

/** Resolve {@link SendTarget} for a layer (§2.2). */
export function sendTarget(layer: Layer): SendTarget {
  switch (layer.kind) {
    case "channel":
      return layer.channelId
        ? { kind: "channel", channelId: layer.channelId }
        : null;
    case "thread":
      return layer.eventId && layer.channelId
        ? {
            kind: "thread",
            rootEventId: layer.eventId,
            channelId: layer.channelId,
          }
        : null;
    case "activity":
      return layer.agentPubkey
        ? { kind: "agent", pubkey: layer.agentPubkey }
        : null;
    default:
      return null;
  }
}

/**
 * Render the breadcrumb — §1.2.
 *
 * Joins the crumbs of every entry with ` › `. Elision is the caller's job
 * ({@link ../render/width.elideFromLeft}) because it needs the available width,
 * which is a render concern rather than a navigation one.
 */
export function breadcrumb(stack: NavStack): string {
  return stack.entries.map((e) => e.crumb).join(" › ");
}
