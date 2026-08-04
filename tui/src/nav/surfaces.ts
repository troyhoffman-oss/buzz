/**
 * Surfaces and the three-tier `Esc` — NAVIGATION.md §5.3.
 *
 * > Ported verbatim from the desktop's `escapeSurfaces.ts` (IA §4), because
 * > getting this wrong breaks mark-as-read, the desktop's most-used key.
 * >
 * > 1. innermost control consumes (completion band, edit-in-progress, text
 * >    selection)
 * > 2. else topmost registered surface closes (modal → drawer expansion →
 * >    drawer → message-select → aux layer)
 * > 3. else global default: mark current channel read
 * >
 * > **Tier 2 is a counter, not z-order, and not a naive focus-stack pop.**
 *
 * The counter distinction is load-bearing and easy to lose. A naive
 * focus-stack pop closes whatever *received focus* last; a counter closes the
 * topmost *registered* surface. They diverge whenever a surface opens without
 * taking focus — the drawer streams live rows while the composer keeps `❯` —
 * and in that state a focus-stack `Esc` would fall through to tier 3 and
 * silently mark the channel read while the user was only trying to close the
 * drawer.
 */

/**
 * A registered surface, innermost-consuming first.
 *
 * The order of this union is the tier-2 close order from §5.3, and
 * {@link topmostSurface} depends on it.
 */
export type SurfaceKind =
  /** `/ @ # : ctrl+f` — the completion band (§2.5). Tier 1: an innermost control. */
  | "completion"
  /** A modal stack over the current layer (§1.3, agents' ~18 dialogs). */
  | "modal"
  /** The drawer's in-place expansion (§2.4). */
  | "drawerExpansion"
  /** The drawer list (§2.3). */
  | "drawer"
  /** Message-select (§3). */
  | "messageSelect";

/**
 * Tier-1 surfaces: an *innermost control* that consumes `Esc` before any
 * surface-closing happens (§5.3 tier 1).
 *
 * The completion band is here rather than in tier 2 because §2.5 makes it a
 * different surface class entirely: it augments what you are typing and leaves
 * the composer live, so its `Esc` belongs to the text you are editing.
 */
const TIER_ONE: readonly SurfaceKind[] = ["completion"];

/** Tier-2 close order (§5.3): modal → drawer expansion → drawer → message-select. */
const TIER_TWO_ORDER: readonly SurfaceKind[] = [
  "modal",
  "drawerExpansion",
  "drawer",
  "messageSelect",
];

/**
 * The set of currently registered surfaces.
 *
 * A **set**, not a stack: §5.3's "a counter, not z-order". Registration order
 * carries no meaning; what closes is decided by the fixed precedence above.
 */
export type SurfaceSet = ReadonlySet<SurfaceKind>;

/** Register a surface. Idempotent — opening an open surface is not an error. */
export function register(set: SurfaceSet, kind: SurfaceKind): SurfaceSet {
  const next = new Set(set);
  next.add(kind);
  return next;
}

/** Unregister a surface. Idempotent. */
export function unregister(set: SurfaceSet, kind: SurfaceKind): SurfaceSet {
  const next = new Set(set);
  next.delete(kind);
  return next;
}

/** How many surfaces are registered — the §5.3 counter itself. */
export function surfaceCount(set: SurfaceSet): number {
  return set.size;
}

/** The surface tier 2 would close, or `null` when none is registered. */
export function topmostSurface(set: SurfaceSet): SurfaceKind | null {
  for (const kind of TIER_TWO_ORDER) if (set.has(kind)) return kind;
  return null;
}

/** What an `Esc` press resolves to, per §5.3's three tiers. */
export type EscapeResolution =
  /** Tier 1 — an innermost control consumed it. */
  | { tier: 1; consumedBy: SurfaceKind }
  /** Tier 2 — close the topmost registered surface. */
  | { tier: 2; close: SurfaceKind }
  /** Tier 3 — the global default: mark the current channel read. */
  | { tier: 3; action: "markChannelRead" };

/**
 * Resolve one `Esc` press (§5.3).
 *
 * Tier 3 is **suppressed while any surface is registered** (§5.4's gating
 * table). That suppression is what makes `Esc` safe to press repeatedly:
 * walking out of a drawer expansion takes two presses and neither of them
 * silently marks a channel read on the way.
 */
export function resolveEscape(set: SurfaceSet): EscapeResolution {
  for (const kind of TIER_ONE) {
    if (set.has(kind)) return { tier: 1, consumedBy: kind };
  }
  const top = topmostSurface(set);
  if (top) return { tier: 2, close: top };
  return { tier: 3, action: "markChannelRead" };
}

/**
 * Whether `Esc` at tier 3 is currently gated (§5.4).
 *
 * Exposed separately from {@link resolveEscape} because the statusline needs to
 * know whether the mark-read affordance is live *before* the key is pressed —
 * an advertised action that will not fire is the §3.4.1 "actionable-looking
 * control that cannot act" failure, applied to a hint row.
 */
export function isMarkReadGated(set: SurfaceSet): boolean {
  return set.size > 0;
}
