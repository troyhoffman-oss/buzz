/**
 * L0 HOME — NAVIGATION.md §1, §4.4, §8 ruling 3.
 *
 * Three zones, top to bottom: **COMMUNITY** (a row, not a layer — §8 ruling 3),
 * **ATTENTION** (whose groups *are* the desktop's eight inbox filters, §1.3),
 * and **PLACES**.
 *
 * §1.1's default selection is the behaviour that makes home worth opening:
 *
 * > the row you last left it from, else the first **attention-bearing** row
 * > (unread, mention, needs-input), else the first row. Home therefore opens on
 * > your top mention when you have one and on PLACES ▸ Channels when you do not.
 *
 * That is why §4.4's "jump to a mention" is two keystrokes rather than five.
 */

import type { AttentionItem, Channel, Community } from "../client/types";
import type { Layer } from "../nav/layers";
import { alignRight, pad, truncateKeepingSuffix } from "../render/width";

/** Order of the ATTENTION groups — the §1 layer map's order, top to bottom. */
export const ATTENTION_GROUPS = [
  "mentions",
  "threads",
  "needsAction",
  "dms",
  "reminders",
  "drafts",
] as const;

/** An ATTENTION group name. */
export type AttentionGroup = (typeof ATTENTION_GROUPS)[number];

/** Display labels for the groups. */
const GROUP_LABELS: Readonly<Record<AttentionGroup, string>> = {
  mentions: "MENTIONS",
  threads: "THREADS",
  needsAction: "NEEDS ACTION",
  dms: "DMs",
  reminders: "REMINDERS",
  drafts: "DRAFTS",
};

/**
 * One row on home.
 *
 * **Zone headers are selectable rows, not chrome**, and that follows from two
 * of the spec's own rules meeting:
 *
 * - §4.1's frame shows `⇧↓` landing on `PLACES` and `↓` then stepping onto
 *   `Channels`. If the zone header could not hold the selection there would be
 *   no visible `❯` between those two presses, breaking [G8].
 * - §5.1 says `⏎` is never wrong. A selectable row with no action would make it
 *   wrong exactly there — so `⏎` on a zone header collapses the whole zone,
 *   which is row 3's "collapse / expand the group" generalized to any header.
 *
 * Treating headers as chrome instead would need a second notion of "row" that
 * the jump respects and the cursor does not, and every rule in §1.1 and §5
 * would then need to say which one it meant.
 */
export type HomeRow =
  | { kind: "zoneHeader"; zone: HomeZone; label: string }
  | { kind: "community"; community: Community }
  | {
      kind: "groupHeader";
      group: AttentionGroup;
      count: number;
      collapsed: boolean;
    }
  | { kind: "attention"; item: AttentionItem }
  | { kind: "place"; place: PlaceId; label: string; status: string };

/** The three zones of home, top to bottom (§1's layer map). */
export type HomeZone = "community" | "attention" | "places";

/** The PLACES rows of the §1 map. Preview-gated rows are absent, not disabled. */
export type PlaceId = "channels" | "agents" | "settings" | "me";

/** State home renders from. */
export interface HomeState {
  readonly communities: readonly Community[];
  readonly attention: readonly AttentionItem[];
  readonly channels: readonly Channel[];
  readonly agentsWorking: number;
  /** Groups the user has collapsed with `⏎` on the header (§4.4). */
  readonly collapsedGroups: ReadonlySet<AttentionGroup>;
  /** Zones the user has collapsed with `⏎` on the zone header. */
  readonly collapsedZones?: ReadonlySet<HomeZone>;
}

/**
 * Build home's rows.
 *
 * Groups with zero items are omitted entirely rather than rendered empty:
 * `REMINDERS 0` is a row you can select, land on, and learn nothing from, and
 * home's whole job is that the first attention-bearing row is near the top.
 */
export function buildHomeRows(state: HomeState): HomeRow[] {
  const rows: HomeRow[] = [];

  // COMMUNITY — a row on home, not an L-1 layer (§8 ruling 3): switching is a
  // scope change, not travel.
  if (state.communities.length > 0) {
    rows.push({ kind: "zoneHeader", zone: "community", label: "COMMUNITY" });
    if (!state.collapsedZones?.has("community")) {
      for (const community of state.communities)
        rows.push({ kind: "community", community });
    }
  }

  const byGroup = new Map<AttentionGroup, AttentionItem[]>();
  for (const item of state.attention) {
    const list = byGroup.get(item.group) ?? [];
    list.push(item);
    byGroup.set(item.group, list);
  }

  const hasAttention = ATTENTION_GROUPS.some(
    (g) => (byGroup.get(g)?.length ?? 0) > 0,
  );
  if (hasAttention) {
    rows.push({ kind: "zoneHeader", zone: "attention", label: "ATTENTION" });
    if (!state.collapsedZones?.has("attention")) {
      for (const group of ATTENTION_GROUPS) {
        const items = byGroup.get(group) ?? [];
        if (items.length === 0) continue;
        const collapsed = state.collapsedGroups.has(group);
        rows.push({
          kind: "groupHeader",
          group,
          count: items.length,
          collapsed,
        });
        if (collapsed) continue;
        for (const item of items) rows.push({ kind: "attention", item });
      }
    }
  }

  rows.push({ kind: "zoneHeader", zone: "places", label: "PLACES" });
  const unread = state.channels.reduce((sum, c) => sum + c.unread, 0);
  const mentions = state.channels.reduce((sum, c) => sum + c.mentions, 0);
  rows.push({
    kind: "place",
    place: "channels",
    label: "Channels",
    status: [
      unread > 0 ? `${unread} unread` : "",
      mentions > 0 ? `${mentions} mention${mentions === 1 ? "" : "s"}` : "",
    ]
      .filter(Boolean)
      .join(" · "),
  });
  rows.push({
    kind: "place",
    place: "agents",
    label: "Agents",
    status: state.agentsWorking > 0 ? `${state.agentsWorking} working` : "",
  });
  rows.push({
    kind: "place",
    place: "settings",
    label: "Settings",
    status: "",
  });
  rows.push({ kind: "place", place: "me", label: "Me", status: "" });

  return rows;
}

/**
 * Whether a row can hold the selection.
 *
 * Every row can, including headers — see {@link HomeRow} for why. Kept as a
 * named function rather than inlined as `true` because "everything is
 * selectable" is a *decision* with a rationale, and a call site that reads
 * `isSelectable(row)` is one that can find it.
 */
export function isSelectable(_row: HomeRow): boolean {
  return true;
}

/**
 * §1.1's default selection, applied to home.
 *
 * > else the first **attention-bearing** row (unread, mention, needs-input),
 * > else the first row. Home therefore opens on your top mention when you have
 * > one and on PLACES ▸ Channels when you do not.
 *
 * The community row is skipped as a default even though it is selectable and
 * sits above ATTENTION: it is a scope switch, and opening home already scoped
 * to that community means it carries no attention.
 */
export function defaultHomeSelection(rows: readonly HomeRow[]): number {
  const attention = rows.findIndex((r) => r.kind === "attention");
  if (attention >= 0) return attention;
  const channels = rows.findIndex(
    (r) => r.kind === "place" && r.place === "channels",
  );
  if (channels >= 0) return channels;
  return rows.findIndex(isSelectable);
}

/**
 * Where `⇧↑`/`⇧↓` land — §1.1's structural jump, applied to a list.
 *
 * > jump by **structural unit** — group header in a list.
 *
 * Zone headers count as structural boundaries alongside group headers; they are
 * not selectable, so the jump lands on the row *after* one, which is what
 * §4.1's `⇧↓` `↓` walkthrough shows: `⇧↓` jumps to PLACES, `↓` steps onto its
 * first row.
 */
export function structuralIndices(rows: readonly HomeRow[]): number[] {
  const out: number[] = [];
  rows.forEach((row, i) => {
    if (row.kind === "zoneHeader" || row.kind === "groupHeader") out.push(i);
  });
  return out;
}

/** Where `→` on a row descends to (§1's layer map). `null` means the row is inert. */
export function descendTarget(row: HomeRow): Layer | null {
  switch (row.kind) {
    case "attention":
      // A teleport: straight to L2 at that message, **skipping L1** (§4.4). The
      // crumb records the route, not the tree position [G15] — which is why `←`
      // returns to the mentions list and not to a channel list never visited.
      return {
        kind: "channel",
        channelId: row.item.channelId,
        eventId: row.item.eventId,
        crumb: row.item.channelName,
        selection: 0,
      };
    case "place":
      if (row.place === "channels")
        return { kind: "channels", crumb: "channels", selection: 0 };
      if (row.place === "agents")
        return { kind: "agents", crumb: "agents", selection: 0 };
      return null;
    default:
      return null;
  }
}

/**
 * The crumb a teleport inserts between `home` and the destination (§4.4).
 *
 * `home › mentions › #engineering`, not `home › channels › #engineering`.
 */
export function teleportCrumb(row: HomeRow): string | null {
  if (row.kind !== "attention") return null;
  return GROUP_LABELS[row.item.group].toLowerCase();
}

/** Render home's rows to text (§4.1's and §4.4's frames). */
export function renderHome(
  rows: readonly HomeRow[],
  selected: number,
  cols: number,
  composerFocused: boolean,
): string[] {
  return rows.map((row, index) => {
    // §2.2's one-glyph rule: the list holds `❯` while the composer is empty; a
    // single typed character moves it and demotes the row to `▌`.
    const marker = index === selected ? (composerFocused ? "▌ " : "❯ ") : "  ";
    // Every row is `marker + body` padded to width, so the marker and the pad
    // are applied once here rather than repeated in five branches — which is
    // what keeps a new row kind from accidentally shipping without one.
    return pad(`${marker}${homeRowBody(row, cols - 2)}`, cols);
  });
}

/** One home row's body, without the focus marker or the trailing pad. */
function homeRowBody(row: HomeRow, cols: number): string {
  switch (row.kind) {
    case "zoneHeader":
      return row.label;
    case "community":
      return alignRight(
        `${row.community.active ? "◉" : "○"} ${row.community.name}`,
        row.community.unread > 0 ? `${row.community.unread} unread` : "",
        cols,
      );
    case "groupHeader":
      return alignRight(
        `${row.collapsed ? "▸" : "▾"} ${GROUP_LABELS[row.group]}`,
        String(row.count),
        cols,
      );
    case "attention":
      return truncateKeepingSuffix(
        `${row.item.author} · ${row.item.channelName}    ${row.item.preview}`,
        formatClock(row.item.ts),
        cols,
      );
    case "place":
      return alignRight(row.label, row.status, cols);
  }
}

/**
 * `HH:MM` in UTC.
 *
 * UTC, not local: §5.3 requirement 5 pins `TZ=UTC` for snapshot determinism,
 * and a renderer that read the ambient zone would produce a suite that passes
 * only in the timezone it was blessed in.
 */
export function formatClock(ts: number): string {
  const date = new Date(ts);
  const hh = String(date.getUTCHours()).padStart(2, "0");
  const mm = String(date.getUTCMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}
