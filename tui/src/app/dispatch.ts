/**
 * Intent → state — NAVIGATION.md §1–§5, the reducer half.
 *
 * `nav/keys.ts` decides *what a key means*; this decides *what that does*. The
 * split is what makes §5's chains testable as tables and §4's walkthroughs
 * testable as key sequences, with neither needing a terminal or a daemon.
 *
 * Every branch below cites the clause it implements, because the interesting
 * cases are all ones where the obvious implementation is subtly wrong: `←`
 * restoring a selection, `⏎` peeking rather than committing, a printable
 * closing a surface and inserting in one keystroke.
 */

import {
  channelRows,
  drawerRows,
  homeRows,
  searchHits,
  selectableCount,
} from "./screen";
import {
  type AppState,
  applyEscape,
  ascend,
  clearComposer,
  collapseHome,
  descend as pushLayer,
  insertChar,
  openDrawer,
  enterMessageSelect,
  selectRow,
  toggleDrawerExpansion,
} from "./state";
import {
  descendTarget as channelDescendTarget,
  structuralIndices as channelStructural,
  defaultChannelSelection,
} from "../layers/channels";
import {
  descendTarget as agentDescendTarget,
  sortFleet,
} from "../layers/agents";
import {
  initialSelectIndex,
  selectedMessage,
  stepSelection,
  stepThreadRoot,
  threadTarget,
} from "../layers/channel";
import {
  defaultHomeSelection,
  descendTarget as homeDescendTarget,
  structuralIndices as homeStructural,
  teleportCrumb,
} from "../layers/home";
import { teleportTarget } from "../layers/search";
import { selectableIds } from "../render/timeline";
import type { Intent } from "../nav/keys";
import { clampSelection } from "../nav/keys";
import {
  type Layer,
  current,
  isChatLayer,
  push,
  sendTarget,
} from "../nav/layers";
import { register, unregister } from "../nav/surfaces";
import {
  applyCompletion,
  detectTrigger,
  rankCandidates,
} from "../render/completion";

/**
 * Descend into a layer, applying §1.1's default selection on arrival.
 *
 * Wrapping `pushLayer` rather than calling it directly is the point: §1.1's
 * default is a property of *entering a layer*, and there are ten descent sites
 * below (home rows, channel rows, fleet rows, drawer promotions, teleports,
 * unread jumps, search). One of them forgetting would produce a layer that
 * opens on row 0 for one route and on its top attention-bearing row for every
 * other — an inconsistency that reads as a bug in the destination rather than
 * in the route that reached it.
 */
function descend(state: AppState, layer: Layer): AppState {
  return withDefaultSelection(pushLayer(state, layer));
}

/**
 * Re-detect the completion trigger against the current composer text — §2.5.
 *
 * Called after every text change. Detection is a **pure function of (text,
 * cursor)** (§3.3's P11 rules), so the band's open/closed state is *derived*
 * rather than tracked: typing `@` opens it, typing more narrows it, deleting
 * back past the `@` closes it, and none of those needs its own branch. A
 * tracked flag would be a second source of truth that can disagree with what
 * the composer actually contains.
 *
 * The selection resets to 0 on every re-filter, matching §3.3's
 * `input: "keyboard"` rule — the previous highlight is meaningless against a
 * new candidate list, and keeping it is how a picker hands you the wrong name.
 */
function syncCompletion(state: AppState): AppState {
  const trigger = detectTrigger(state.composer, state.cursor);
  if (!trigger) {
    if (!state.completion) return state;
    return {
      ...state,
      completion: null,
      surfaces: unregister(state.surfaces, "completion"),
    };
  }
  // Only the mention band is wired in Wave 1; `#`, `:` and `/` detect but do
  // not yet render, and opening an empty band would be worse than not opening
  // one — §3.4.1's "an actionable-looking control that cannot act is worse than
  // no control", applied to a completion surface.
  if (trigger.kind !== "mention") {
    return state.completion
      ? {
          ...state,
          completion: null,
          surfaces: unregister(state.surfaces, "completion"),
        }
      : state;
  }
  return {
    ...state,
    completion: {
      kind: trigger.kind,
      query: trigger.query,
      triggerAt: trigger.at,
      agentsOnly: trigger.agentsOnly,
      selection: 0,
    },
    surfaces: register(state.surfaces, "completion"),
  };
}

/** Messages on the current layer's channel, oldest first, top-level only. */
function timelineMessages(state: AppState) {
  const layer = current(state.stack);
  if (!layer.channelId) return [];
  return [...(state.snapshot.messages[layer.channelId] ?? [])]
    .filter((m) => !m.replyTo)
    .sort((a, b) => a.ts - b.ts);
}

/**
 * Structural-jump targets for the current layer — §1.1's `⇧↑`/`⇧↓`.
 *
 * > one gesture, one meaning: skip everything that is not a significant
 * > boundary — **group header in a list, thread root in a timeline**.
 */
function structuralTargets(state: AppState): number[] {
  const layer = current(state.stack);
  switch (layer.kind) {
    case "home":
      return homeStructural(homeRows(state));
    case "channels":
      return channelStructural(channelRows(state));
    default:
      return [];
  }
}

/** Move a list selection, clamped, no wrap [G4]. */
function moveSelection(
  state: AppState,
  delta: number,
  cols: number,
  now: number,
): AppState {
  const layer = current(state.stack);

  if (state.drawer) {
    const count = drawerRows(state).length;
    return {
      ...state,
      drawer: {
        ...state.drawer,
        selection: clampSelection(state.drawer.selection + delta, count),
      },
    };
  }

  if (state.messageSelect) {
    const messages = timelineMessages(state);
    return {
      ...state,
      messageSelect: {
        index: stepSelection(messages, state.messageSelect.index, delta),
      },
    };
  }

  // Every row on every list layer can hold the selection — headers included
  // (see `isSelectable` in `layers/home.ts` and `layers/channels.ts`). So the
  // step is a plain clamped move with no skip logic, which is what makes
  // §4.1's `⇧↓` `↓` sequence mean exactly what it reads as: the jump lands on
  // the header, the `↓` steps onto the row below it.
  const count = selectableCount(state, cols, now);
  return selectRow(state, clampSelection(layer.selection + delta, count));
}

/** `⇧↑`/`⇧↓` — jump by structural unit (§1.1, §3). */
function jumpStructural(state: AppState, delta: number): AppState {
  if (state.messageSelect) {
    const messages = timelineMessages(state);
    return {
      ...state,
      messageSelect: {
        index: stepThreadRoot(messages, state.messageSelect.index, delta),
      },
    };
  }
  const targets = structuralTargets(state);
  if (targets.length === 0) return state;
  const layer = current(state.stack);
  const step = delta < 0 ? -1 : 1;
  const candidates = step < 0 ? [...targets].reverse() : targets;
  const next = candidates.find((i) =>
    step < 0 ? i < layer.selection : i > layer.selection,
  );
  // Clamp at the ends rather than wrapping — [G4]'s "boundaries are never
  // trapdoors", applied to the jump: `⇧↑` at the first section stays put.
  if (next === undefined) return state;
  // Land on the header's first *selectable* row, which is what §4.1 shows:
  // "`⇧↓` jumps to the next group header (PLACES); `↓` steps onto its first
  // row." The header itself is selectable for `⏎` to toggle it, so the jump
  // lands on the header and the following `↓` does the stepping.
  return selectRow(state, next);
}

/** `→` / `⏎` — commit, descending one layer (§1.1). */
function commit(state: AppState): AppState {
  const layer = current(state.stack);

  // From the drawer, `→` **promotes** the peek to a full layer (§2.4). The
  // drawer stays warm in the sense §5.4 means it: `←` from the promoted layer
  // returns, because the back stack remembered the path in [G15].
  if (state.drawer) {
    const row = drawerRows(state)[state.drawer.selection];
    if (!row) return state;
    const target = row.target;
    if (target.kind === "activity") {
      return descend(state, {
        kind: "activity",
        agentPubkey: target.agentPubkey,
        ...(target.channelId ? { channelId: target.channelId } : {}),
        crumb: row.label,
        selection: 0,
      });
    }
    if (target.kind === "thread") {
      return descend(state, {
        kind: "thread",
        channelId: target.channelId,
        eventId: target.rootEventId,
        crumb: `⤷ ${row.label}`,
        selection: 0,
      });
    }
    return state;
  }

  // From message-select, `→`/`⏎` opens the thread — §8 ruling 2 makes this the
  // only route from a timeline into a thread.
  if (state.messageSelect) {
    const messages = timelineMessages(state);
    const message = selectedMessage(messages, state.messageSelect.index);
    if (!message) return state;
    return descend(state, threadTarget(message));
  }

  switch (layer.kind) {
    case "home": {
      const rows = homeRows(state);
      const row = rows[layer.selection];
      if (!row) return state;
      const target = homeDescendTarget(row);
      if (!target) return state;
      // A teleport inserts the group it came from, so the crumb records the
      // route rather than the tree (§4.4, [G15]).
      //
      // The seeded entry is **`home` scoped to the group**, not `channels`.
      // §4.4 is explicit that "`←` returns to the mentions list, not to a
      // channel list you never visited" — seeding a `channels` layer produced a
      // crumb reading `home › mentions` above a rendered channel list, which is
      // the exact confusion the rule forbids, wearing the right label.
      const via = teleportCrumb(row);
      if (via) {
        const seeded = descend(state, {
          kind: "home",
          crumb: via,
          selection: layer.selection,
        });
        return { ...seeded, stack: push(seeded.stack, target) };
      }
      return descend(state, target);
    }
    case "channels": {
      const row = channelRows(state)[layer.selection];
      if (!row) return state;
      // `isDestination` rather than `isSelectable`: a section header can hold
      // `❯` and can be collapsed with `⏎`, but `→` on it must not descend into
      // an arbitrary member of the section.
      const target = channelDescendTarget(row);
      return target ? descend(state, target) : state;
    }
    case "agents": {
      const agent = sortFleet(state.snapshot.agents)[layer.selection];
      return agent ? descend(state, agentDescendTarget(agent)) : state;
    }
    case "results": {
      // The hits are re-derived from the same (query, snapshot) pair the screen
      // rendered from, so `→` lands on the row the user is looking at rather
      // than on a stale index — the results list has no state of its own.
      const hit = searchHits(state)[layer.selection];
      if (!hit) return state;
      // A teleport, exactly as §4.4's mention route is: straight to L2 at the
      // message, with the back stack seeded so `←` returns to the results you
      // were reading rather than to a channel list you never opened [G15].
      return descend(state, teleportTarget(hit));
    }
    case "channel": {
      // With no message-select active there is nothing selected to descend
      // into. §3 is explicit that message-select is the entry to thread
      // descent, so `→` here enters it rather than doing nothing — a key that
      // silently no-ops is the one outcome §5.1 rules out.
      const messages = timelineMessages(state);
      if (messages.length === 0) return state;
      return enterMessageSelect(state, initialSelectIndex(messages));
    }
    default:
      return state;
  }
}

/** Apply one resolved intent. */
export function applyIntent(
  state: AppState,
  intent: Intent,
  cols: number,
  now: number,
): AppState {
  switch (intent.kind) {
    case "moveSelection":
      return moveSelection(state, intent.delta, cols, now);

    case "jumpStructural":
      return jumpStructural(state, intent.delta);

    case "enterMessageSelect": {
      const messages = timelineMessages(state);
      if (messages.length === 0) return state;
      return enterMessageSelect(state, initialSelectIndex(messages));
    }

    case "openDrawer":
      return openDrawer(state);

    case "peek":
      return toggleDrawerExpansion(state);

    case "openThread": {
      const messages = timelineMessages(state);
      if (!state.messageSelect) return state;
      const message = selectedMessage(messages, state.messageSelect.index);
      if (!message) return state;
      return descend(state, threadTarget(message));
    }

    case "descend":
      return commit(state);

    case "ascend":
      return ascend(state);

    case "goHome":
      return collapseHome(state);

    case "escape":
      return applyEscape(state);

    case "insertChar":
      // Typing at L1 changes what the list *is* (§8 ruling 1's fuzzy filter),
      // so the selection has to be re-derived against the new rows. Carrying
      // the old index over would leave `❯` on row 3 of a list that now has one
      // row — where `→` and `⏎` both silently do nothing, because the index
      // resolves to no row at all. That is the "key that sometimes does
      // nothing" outcome §5.1 rules out.
      //
      // `syncCompletion` then re-detects the trigger against the new text, so
      // the band opens on `@`, follows the query, and closes when the trigger
      // is deleted — all from one pure function of (text, cursor) rather than
      // from an open/close flag that could disagree with what is on screen.
      return syncCompletion(
        withDefaultSelection(insertChar(state, intent.char)),
      );

    case "completionAccept": {
      if (!state.completion) return state;
      const candidates = rankCandidates(
        state.snapshot.mentionCandidates,
        state.completion.query,
        state.completion.agentsOnly,
      );
      const picked = candidates[state.completion.selection];
      // §3.3: "Enter with zero candidates sends nothing and inserts nothing. It
      // is a no-op that keeps the popup open with a `no matches` footer." The
      // worst outcome here is a half-composed message sent by a reflexive
      // Enter, so the guard is load-bearing rather than defensive.
      if (!picked) return state;
      const trigger = {
        kind: state.completion.kind,
        at: state.completion.triggerAt,
        query: state.completion.query,
        agentsOnly: state.completion.agentsOnly,
      };
      const { text, cursor } = applyCompletion(
        state.composer,
        trigger,
        `@${picked.handle}`,
      );
      return {
        ...state,
        composer: text,
        cursor,
        completion: null,
        surfaces: unregister(state.surfaces, "completion"),
        // [D-2]: the composer's parts hold the **pubkey**, so "what you picked
        // is what gets tagged" is true by construction rather than by two
        // implementations agreeing.
        mentions: [...state.mentions, picked.pubkey],
      };
    }

    case "completionMove": {
      if (!state.completion) return state;
      const count = rankCandidates(
        state.snapshot.mentionCandidates,
        state.completion.query,
        state.completion.agentsOnly,
      ).length;
      return {
        ...state,
        completion: {
          ...state.completion,
          selection: clampSelection(
            state.completion.selection + intent.delta,
            count,
          ),
        },
      };
    }

    case "completionClose":
      // §3.3: "`esc` dismisses to literal text" — the typed `@ma` stays in the
      // composer as characters. Deleting it would lose a keystroke, which [G5]
      // forbids anywhere in this design.
      return {
        ...state,
        completion: null,
        surfaces: unregister(state.surfaces, "completion"),
      };

    case "moveTextCursor":
      return {
        ...state,
        cursor: clampSelection(
          state.cursor + intent.delta,
          state.composer.length + 1,
        ),
      };

    case "moveTextEdge":
      return {
        ...state,
        cursor: intent.edge === "start" ? 0 : state.composer.length,
      };

    case "scrollTail": {
      if (!state.drawer) return state;
      // Offset is measured **from the newest line**, so a live tail that is
      // pinned (offset 0) keeps advancing while a scrolled-back one holds its
      // position as rows arrive — the "advances live while you read" property
      // §2.4 claims.
      return {
        ...state,
        drawer: {
          ...state.drawer,
          tailOffset: Math.max(0, state.drawer.tailOffset - intent.delta),
        },
      };
    }

    case "send": {
      const target = sendTarget(current(state.stack));
      if (!target) {
        // §8 ruling 1: at L1 the composer is jump/filter only, so `⏎` with text
        // enters the highlighted row rather than posting to it.
        return commit(state);
      }
      const channelId =
        target.kind === "channel"
          ? target.channelId
          : target.kind === "thread"
            ? target.channelId
            : null;
      if (!channelId) return state;
      return {
        ...clearComposer(state),
        pending: {
          kind: "send",
          channelId,
          content: state.composer,
          // [D-2]: resolved pubkeys picked in the completion band, not names
          // re-extracted from the text at send time.
          mentions: state.mentions,
          ...(target.kind === "thread" ? { replyTo: target.rootEventId } : {}),
        },
      };
    }

    case "toggleGroup": {
      // §5.1 row 3, generalized: `⏎` on any header collapses what it heads —
      // a group header its group, a zone header its whole zone. Collapsing
      // groups is what replaces the desktop's InboxFilterMenu entirely (§4.4).
      const layer = current(state.stack);
      const row = homeRows(state)[layer.selection];
      if (row?.kind === "groupHeader") {
        const next = new Set(state.collapsedGroups);
        if (next.has(row.group)) next.delete(row.group);
        else next.add(row.group);
        return { ...state, collapsedGroups: next };
      }
      if (row?.kind === "zoneHeader") {
        const next = new Set(state.collapsedZones);
        if (next.has(row.zone)) next.delete(row.zone);
        else next.add(row.zone);
        return { ...state, collapsedZones: next };
      }
      return state;
    }

    case "openSearch":
      // `ctrl+k` teleports and seeds the back stack (§1.4). Entering as an L1
      // layer rather than as a modal is what makes `←` return to where you
      // searched from.
      return descend(state, {
        kind: "results",
        crumb: "search",
        query: "",
        selection: 0,
      });

    case "findInChannel":
      // §2.5: a completion-class band, **not a layer** — the composer and
      // statusline stay intact and live beneath it.
      return { ...state, surfaces: register(state.surfaces, "completion") };

    case "markAllRead":
      return { ...state, pending: { kind: "markAllRead" } };

    case "unreadJump": {
      // `ctrl+↑`/`ctrl+↓` replace the desktop's MoreUnreadButton (§1.4). With
      // one unread channel this is a jump into it; the general case walks the
      // unread set in list order.
      const unread = state.snapshot.channels.filter((c) => c.unread > 0);
      if (unread.length === 0) return state;
      const layer = current(state.stack);
      const at = unread.findIndex((c) => c.id === layer.channelId);
      const next =
        unread[clampSelection(at + intent.delta, unread.length)] ?? unread[0];
      if (!next) return state;
      return descend(state, {
        kind: "channel",
        channelId: next.id,
        crumb: next.name,
        selection: 0,
      });
    }

    default:
      return state;
  }
}

/**
 * Apply §1.1's default selection to the current layer.
 *
 * > the row you last left it from, else the first **attention-bearing** row
 * > (unread, mention, needs-input), else the first row. **Home therefore opens
 * > on your top mention when you have one** and on PLACES ▸ Channels when you
 * > do not.
 *
 * That last sentence is why §4.4's "jump to a mention" is two keystrokes rather
 * than five, and it only holds if this runs at **boot** as well as on descent —
 * a home that opens on the COMMUNITY row makes the shortest, most-used path in
 * the design four presses longer.
 *
 * Only ever applied to a *newly entered* layer. A layer returned to via `←`
 * keeps the selection it was left with, which is §1.1's "descend and ascend are
 * inverses" and the reason the spine reads as spatial.
 */
export function withDefaultSelection(state: AppState): AppState {
  const layer = current(state.stack);
  switch (layer.kind) {
    case "home":
      return selectRow(state, defaultHomeSelection(homeRows(state)));
    case "channels":
      return selectRow(state, defaultChannelSelection(channelRows(state)));
    case "results":
      // A re-filtered result list is a new list; keeping the old index would
      // leave `❯` past its end, where `→` teleports nowhere. Same reasoning as
      // the L1 channel filter.
      return selectRow(state, 0);
    default:
      return state;
  }
}

/** Re-exported for the tests that assert the timeline's select axis. */
export { selectableIds, isChatLayer, teleportTarget };
