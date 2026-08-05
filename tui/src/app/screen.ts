/**
 * `AppState` → the screen — NAVIGATION.md §1–§6, assembled.
 *
 * One pure function, `renderScreen(state, cols, rows, now) → string[]`. It is
 * the single place the layer spine, the bottom bands, and the surfaces meet,
 * and keeping it pure is what makes the T1 matrix and the §4 walkthrough tests
 * possible without a terminal.
 *
 * The two invariants it enforces on every path:
 *
 * - **Exactly one `❯` on screen** [G8]. Which element holds it is *derived*
 *   from (layer class, composer emptiness, active surface), never toggled —
 *   {@link composerOwnsFocus} is the whole rule.
 * - **The bottom bands are the only chrome.** No side-by-side panes at any
 *   width (§1), so there is no layout branch on `cols` anywhere below: width
 *   reaches the renderers as a parameter and comes out as different cuts of the
 *   same content (§7).
 */

import { renderMentionPicker, rankCandidates } from "../render/completion";
import {
  type DrawerRow,
  buildDrawerRows,
  drawerListHeight,
  expansionHeight,
  renderDrawerExpansion,
  renderDrawerList,
} from "../render/drawer";
import { renderStatusline } from "../render/statusline";
import { renderTimeline, selectableIds } from "../render/timeline";
import {
  renderTranscript,
  activityFields,
  renderFleet,
  sortFleet,
} from "../layers/agents";
import {
  buildChannelRows,
  renderChannels,
  type ChannelRow,
} from "../layers/channels";
import { messageSelectHints, selectedMessage } from "../layers/channel";
import { buildHomeRows, renderHome, type HomeRow } from "../layers/home";
import { buildThread, renderThread } from "../layers/thread";
import { localSearch, parseQuery, renderResults } from "../layers/search";
import { FOCUS_GLYPH, POSITION_GLYPH } from "../render/bands";
import { type EmptyContext, emptyStateRows } from "../render/emptystate";
import { pad } from "../render/width";
import { type BottomBand, renderFrame } from "../shell/frame";
import { MIN_COLS, MIN_ROWS, isBelowFloor } from "../shell/tiers";
import {
  composerPlaceholder,
  current,
  isChatLayer,
  breadcrumb,
} from "../nav/layers";
import type { AppState } from "./state";

/**
 * Who holds `❯` — §2.2's one-glyph rule, as a single derivation.
 *
 * > - **Chat layers (L2, L3, L4)** — the composer owns `❯` by default.
 * > - **Picker layers (L0, L1)** — the list owns `❯` while the composer is
 * >   empty; type one character and `❯` moves to the composer, the row demoting
 * >   to `▌`.
 *
 * The drawer and message-select take `❯` away from the composer while they are
 * open, because they replace it entirely (§2.3, §3) — the composer is not on
 * screen to hold anything.
 */
export function composerOwnsFocus(state: AppState): boolean {
  if (state.drawer || state.messageSelect) return false;
  if (isChatLayer(current(state.stack).kind)) return true;
  // **An empty picker list cannot hold the glyph.** [G8] says there is exactly
  // one `❯` on screen and that it marks where keys go; a picker layer with no
  // rows has nowhere to put it, so leaving the rule at "the list owns it while
  // the composer is empty" renders **zero** — measured in the live L1 CHANNELS
  // and L1 AGENTS captures, where the daemon legitimately has nothing to list.
  //
  // Zero is the worse failure of the two the rule guards against: two glyphs
  // are confusing, none says the keyboard goes nowhere, on the exact screen an
  // operator is trying to type a filter into. Focus falls back to the composer,
  // which is both on screen and the only thing that can still act.
  if (pickerListIsEmpty(state)) return true;
  return state.composer.length > 0;
}

/**
 * Whether the current picker layer has no rows at all to select.
 *
 * Deliberately the *source* list rather than the filtered one: typing a query
 * that matches nothing already moves `❯` to the composer by the rule below,
 * because the composer is non-empty. It is the unfiltered-and-still-empty case
 * that has no owner for the glyph.
 */
function pickerListIsEmpty(state: AppState): boolean {
  switch (current(state.stack).kind) {
    case "channels":
      return state.snapshot.channels.length === 0;
    case "agents":
      return state.snapshot.agents.length === 0;
    default:
      return false;
  }
}

/** The rows the current layer's body renders to, plus its selection axis. */
interface Body {
  readonly rows: readonly string[];
  readonly anchor: "top" | "bottom";
  /** How many selectable rows the layer has, for clamping. */
  readonly count: number;
  /**
   * Body row index the viewport must keep visible — see `FrameInput.follow`.
   *
   * Derived by finding the rendered row that carries the focus marker rather
   * than by re-deriving the selection's position arithmetically. The renderer
   * already decided where the marker went (a wrapped message body, a diff card,
   * a group header all occupy different numbers of rows), and a second
   * calculation would be a second source of truth that drifts the moment any
   * row changes height.
   */
  readonly follow?: number;
}

/**
 * The `follow` index for a rendered body: the row carrying the focus marker.
 *
 * Returns an empty object when nothing is marked, so it can be spread into a
 * {@link Body} under `exactOptionalPropertyTypes` without an explicit
 * `undefined`.
 */
function withFollow(rows: readonly string[]): { follow?: number } {
  const index = rows.findIndex(
    (row) => row.startsWith(FOCUS_GLYPH) || row.startsWith(POSITION_GLYPH),
  );
  return index >= 0 ? { follow: index } : {};
}

/**
 * The two §5.1 predicates that depend on *what row is selected* — rows 3 and 5.
 *
 * ```
 * 3. group header selected                   → collapse / expand the group
 * 5. L0 attention row selected               → PEEK: expand in place
 * ```
 *
 * Derived here rather than in the Shell because the answer is a function of the
 * rendered row list, and the Shell does not build one. Duplicating the
 * derivation there would let the key chain and the screen disagree about which
 * row is selected — the [G8] "exactly one `❯`" invariant broken from the inside,
 * where no snapshot would catch it.
 */
export function rowContext(state: AppState): {
  groupHeaderSelected: boolean;
  attentionRowSelected: boolean;
} {
  const layer = current(state.stack);
  if (layer.kind !== "home") {
    return { groupHeaderSelected: false, attentionRowSelected: false };
  }
  const row = homeRows(state)[layer.selection];
  return {
    // Zone headers count as group headers for §5.1 row 3: `⏎` on `ATTENTION`
    // collapses the zone the same way `⏎` on `MENTIONS` collapses the group.
    // Anything else would make `⏎` inert on a row that can hold `❯`, and §5.1
    // is explicit that `⏎` is never wrong.
    groupHeaderSelected:
      row?.kind === "groupHeader" || row?.kind === "zoneHeader",
    attentionRowSelected: row?.kind === "attention",
  };
}

/** Home's rows, cached per render so selection and rendering agree. */
export function homeRows(state: AppState): HomeRow[] {
  return buildHomeRows({
    communities: state.snapshot.communities,
    attention: state.snapshot.attention,
    channels: state.snapshot.channels,
    agentsWorking: state.snapshot.agents.filter((a) => a.state === "working")
      .length,
    collapsedGroups: state.collapsedGroups as ReadonlySet<never>,
    collapsedZones: state.collapsedZones as ReadonlySet<never>,
  });
}

/** The channel list's rows for the current filter (§8 ruling 1). */
export function channelRows(state: AppState): ChannelRow[] {
  return buildChannelRows(state.snapshot.channels, state.composer);
}

/**
 * The current search's hits — DESIGN §3.5, NAVIGATION §7.
 *
 * Exported so the renderer and the `→` teleport derive them from the same
 * (query, snapshot) pair. Two derivations would let the row you are looking at
 * and the row you land on diverge, which on a teleport is the worst kind of
 * navigation bug: silent, and only reproducible against a specific corpus.
 *
 * The composer **is** the query while you are on the results layer — §3.5's
 * search-as-you-type. `layer.query` is only the seed a deep link or a
 * `ctrl+k`-with-selection arrives with, so it is used when the composer is
 * empty and overridden the moment you type. Reading `layer.query ?? composer`
 * instead would never reach the composer at all, because the layer is created
 * carrying `query: ""` — a real bug that renders every message in the community
 * as a "result".
 *
 * An empty query returns **nothing**, not everything: a result list showing
 * every message you have is not a search, and it would make `→` teleport to
 * whatever sorted first.
 */
export function searchQuery(state: AppState): string {
  const layer = current(state.stack);
  return state.composer.length > 0 ? state.composer : (layer.query ?? "");
}

export function searchHits(state: AppState) {
  const query = searchQuery(state);
  if (query.trim().length === 0) return [];
  const names = new Map(state.snapshot.channels.map((c) => [c.id, c.name]));
  const all = Object.values(state.snapshot.messages).flat();
  return localSearch(all, names, parseQuery(query));
}

/** The drawer's rows, in [G12] ladder order. */
export function drawerRows(state: AppState): DrawerRow[] {
  const system: string[] = [];
  const connection = state.snapshot.session.connection;
  if (connection.state === "reconnecting") {
    system.push(`relay reconnecting (attempt ${connection.attempt})`);
  }
  if (!state.snapshot.session.archiving)
    system.push("daemon has no identity — not archiving");
  return buildDrawerRows(
    state.snapshot.agents,
    state.snapshot.threads,
    state.snapshot.huddles,
    system,
  );
}

/** Messages for the current layer's channel, oldest first. */
function channelMessages(state: AppState) {
  const layer = current(state.stack);
  if (!layer.channelId) return [];
  return [...(state.snapshot.messages[layer.channelId] ?? [])]
    .filter((m) => !m.replyTo)
    .sort((a, b) => a.ts - b.ts);
}

/**
 * What an empty list needs to explain itself (§1.3 property 3).
 *
 * Both halves come off the snapshot rather than being re-derived from the
 * absence of rows, because the absence is exactly what cannot tell the three
 * causes apart — see `render/emptystate.ts`.
 */
function emptyContext(state: AppState): EmptyContext {
  return {
    connection: state.snapshot.session.connection,
    missing: state.snapshot.missing ?? [],
  };
}

function renderBody(state: AppState, cols: number, now: number): Body {
  const layer = current(state.stack);
  const focused = composerOwnsFocus(state);

  switch (layer.kind) {
    case "home": {
      const rows = homeRows(state);
      const rendered = renderHome(rows, layer.selection, cols, focused);
      return {
        rows: rendered,
        anchor: "top",
        count: rows.length,
        ...withFollow(rendered),
      };
    }
    case "channels": {
      const rows = channelRows(state);
      // Two different emptinesses, and conflating them is the bug §1.3
      // property 3 describes. A filter that matched nothing is a statement
      // about the *query* — the list is fine, your three letters missed — and
      // it must not accuse the transport. Only an empty source list can be
      // caused by the daemon.
      if (state.snapshot.channels.length === 0) {
        const rendered = emptyStateRows(
          "channels",
          "/channel",
          emptyContext(state),
          cols,
        );
        return { rows: rendered, anchor: "top", count: 0 };
      }
      if (rows.length === 0) {
        return {
          rows: [pad("  no channels match", cols)],
          anchor: "top",
          count: 0,
        };
      }
      const rendered = renderChannels(rows, layer.selection, cols, focused);
      return {
        rows: rendered,
        anchor: "top",
        count: rows.length,
        ...withFollow(rendered),
      };
    }
    case "agents": {
      const agents = sortFleet(state.snapshot.agents);
      if (agents.length === 0) {
        const rendered = emptyStateRows(
          "agents",
          "/agent/fleet",
          emptyContext(state),
          cols,
        );
        return { rows: rendered, anchor: "top", count: 0 };
      }
      const rendered = renderFleet(agents, layer.selection, cols, now, focused);
      return {
        rows: rendered,
        anchor: "top",
        count: agents.length,
        ...withFollow(rendered),
      };
    }
    case "results": {
      const hits = searchHits(state);
      const rendered = renderResults(
        hits,
        layer.selection,
        cols,
        now,
        focused,
        searchQuery(state).trim().length > 0,
      );
      return {
        rows: rendered,
        anchor: "top",
        count: hits.length,
        ...withFollow(rendered),
      };
    }
    case "channel": {
      const messages = channelMessages(state);
      const selectedId = state.messageSelect
        ? selectableIds(messages)[state.messageSelect.index]
        : undefined;
      const rendered = renderTimeline(messages, {
        cols,
        // Anchored to an **event id**, never a row index (§3.1) — which is
        // what lets it survive a reconnect burst and a re-render.
        unreadAfterEventId: layer.channelId
          ? state.snapshot.readMarkers?.[layer.channelId]
          : undefined,
        ...(selectedId !== undefined ? { selectedMessageId: selectedId } : {}),
      }).map((r) => r.text);
      return {
        rows: rendered,
        anchor: "bottom",
        count: selectableIds(messages).length,
        // Only follow while message-select is active. Without the guard the
        // timeline would pin to whatever row happened to start with a marker
        // glyph, and a channel with the composer resident must stay anchored to
        // the newest message.
        ...(state.messageSelect ? withFollow(rendered) : {}),
      };
    }
    case "thread": {
      const all = layer.channelId
        ? (state.snapshot.messages[layer.channelId] ?? [])
        : [];
      const root = all.find((m) => m.id === layer.eventId);
      if (!root)
        return { rows: ["  thread not loaded"], anchor: "bottom", count: 0 };
      const nodes = buildThread(root, all);
      return {
        rows: renderThread(nodes, cols),
        anchor: "bottom",
        count: nodes.length,
      };
    }
    case "activity": {
      const rows = layer.agentPubkey
        ? (state.snapshot.transcripts[layer.agentPubkey] ?? [])
        : [];
      const agent = state.snapshot.agents.find(
        (a) => a.pubkey === layer.agentPubkey,
      );
      const usage = layer.agentPubkey
        ? state.snapshot.usage[layer.agentPubkey]
        : undefined;
      const header = agent
        ? activityFields(agent, usage, now).map(([k, v]) => `  ${k}: ${v}`)
        : [];
      // §6's one renderable: the exact same `renderTranscript` the drawer's
      // expansion calls. Different entry, different back target, same rows.
      return {
        rows: [...header, "", ...renderTranscript(rows, cols)],
        anchor: "bottom",
        count: rows.length,
      };
    }
  }
}

/** The bottom band: composer, or whatever replaced it (§2.3, §3). */
function renderBottom(
  state: AppState,
  cols: number,
  rows: number,
  now: number,
): { band: BottomBand; statusline: string[] } {
  const layer = current(state.stack);

  if (state.drawer) {
    // The drawer replaces composer *and* statusline (§2.3) — it does not
    // overlay, and the body above it is compressed, never evicted [G16].
    const all = drawerRows(state);
    // Content-derived, capped by §2.4's `min(18, floor(H * 0.5))`. The cap is
    // what keeps the chat "compressed, never evicted" [G16] on a short
    // terminal; deriving from content is what stops a tall one degrading the
    // drawer for space it is not short of.
    const bandRows = Math.min(
      drawerListHeight(all, cols),
      expansionHeight(rows),
    );
    if (state.drawer.expanded) {
      const row = all[state.drawer.selection];
      // Narrow through a local, so the discriminant survives the property
      // access — `row?.target.kind === "activity"` narrows `row.target` only
      // inside the same expression, not across the `.find` callback.
      const target = row?.target;
      const agent =
        target?.kind === "activity"
          ? state.snapshot.agents.find((a) => a.pubkey === target.agentPubkey)
          : undefined;
      const usage = agent ? state.snapshot.usage[agent.pubkey] : undefined;
      const transcript = agent
        ? (state.snapshot.transcripts[agent.pubkey] ?? [])
        : [];
      const tail = renderTranscript(transcript, Math.max(1, cols - 6));
      return {
        band: {
          kind: "replaced",
          rows: renderDrawerExpansion(
            {
              title: agent ? "Agent activity" : (row?.label ?? "Detail"),
              fields: agent ? activityFields(agent, usage, now) : [],
              tail,
              tailOffset: state.drawer.tailOffset,
              tailTotal: tail.length,
            },
            cols,
            rows,
          ),
        },
        statusline: [],
      };
    }
    return {
      band: {
        kind: "replaced",
        rows: renderDrawerList(all, state.drawer.selection, cols, bandRows),
      },
      statusline: [],
    };
  }

  if (state.messageSelect) {
    // Message-select is the drawer's mirror image (§3): it replaces the same
    // two bands with a hint footer, and is reversible the same way.
    return {
      band: { kind: "replaced", rows: messageSelectHints(cols) },
      statusline: [],
    };
  }

  const agentsHere = layer.channelId
    ? state.snapshot.agents.filter(
        (a) => a.channelId === layer.channelId && a.state !== "idle",
      ).length
    : undefined;

  return {
    band: {
      kind: "composer",
      text: state.composer,
      placeholder: composerPlaceholder(layer),
      focused: composerOwnsFocus(state),
    },
    statusline: renderStatusline(
      {
        relayUrl: state.snapshot.session.relayUrl,
        identity: state.snapshot.session.name,
        scope: layer.crumb,
        connection: state.snapshot.session.connection,
        archiving: state.snapshot.session.archiving,
        meter: attentionMeter(state),
        unread: state.snapshot.channels.reduce((sum, c) => sum + c.unread, 0),
        mentions: state.snapshot.channels.reduce(
          (sum, c) => sum + c.mentions,
          0,
        ),
        dms: state.snapshot.channels.filter(
          (c) => c.kind === "dm" && c.unread > 0,
        ).length,
        agentsWorking: state.snapshot.agents.filter((a) => a.state !== "idle")
          .length,
        ...(agentsHere !== undefined ? { agentsWorkingHere: agentsHere } : {}),
        huddles: state.snapshot.huddles.length,
        chatLayer: isChatLayer(layer.kind),
      },
      cols,
    ),
  };
}

/**
 * The attention meter's fill (§2.1 row 2).
 *
 * Attention-weighted rather than volume-weighted: a mention counts for more
 * than an unread, and an agent needing input counts for more than either.
 * A meter that filled with unread volume would sit at 100% in a busy community
 * and stop carrying information, which is the failure mode a meter has.
 */
function attentionMeter(state: AppState): number {
  const mentions = state.snapshot.channels.reduce(
    (sum, c) => sum + c.mentions,
    0,
  );
  const needsInput = state.snapshot.agents.filter(
    (a) => a.state === "needsInput",
  ).length;
  const unread = state.snapshot.channels.reduce((sum, c) => sum + c.unread, 0);
  const weighted = needsInput * 8 + mentions * 4 + Math.min(unread, 20);
  return Math.min(1, weighted / 40);
}

/**
 * Render the whole screen.
 *
 * Below the floor it renders **one legible line and keeps running** — it does
 * not exit and does not panic (§3.9's surviving clause). Rendering into a
 * zero-size rect is a no-op, never a crash.
 */
export function renderScreen(
  state: AppState,
  cols: number,
  rows: number,
  now: number,
): string[] {
  if (cols <= 0 || rows <= 0) return [];
  if (isBelowFloor(cols, rows)) {
    return [`terminal too small — ${MIN_COLS}x${MIN_ROWS} minimum`];
  }

  const body = renderBody(state, cols, now);
  const { band, statusline } = renderBottom(state, cols, rows, now);

  // §2.5: the completion band renders **above the top rule**, leaving the
  // composer and statusline fully intact and live. It is a different surface
  // class from the drawer and the two are never merged.
  const completion = state.completion
    ? renderMentionPicker(
        rankCandidates(
          state.snapshot.mentionCandidates,
          state.completion.query,
          state.completion.agentsOnly,
        ),
        state.completion.selection,
        cols,
      )
    : undefined;

  return renderFrame({
    cols,
    rows,
    body: body.rows,
    bodyAnchor: body.anchor,
    crumb: breadcrumb(state.stack),
    bottom: band,
    statusline,
    ...(body.follow !== undefined ? { follow: body.follow } : {}),
    ...(completion ? { completion } : {}),
  });
}

/** The number of selectable rows on the current layer, for clamping [G4]. */
export function selectableCount(
  state: AppState,
  cols: number,
  now: number,
): number {
  return renderBody(state, cols, now).count;
}
