/**
 * Search — NAVIGATION.md §1.4, §7, DESIGN.md §3.5.
 *
 * §7 supersedes §3.5 on *surface* only:
 *
 * > **§3.5 Search** — superseded on surface. In-channel find is `ctrl+f` as a
 * > completion band; global search is `ctrl+k` → L1 RESULTS → teleport. Query
 * > syntax retained.
 *
 * So the operator parsing below is `parseSearchOperators.ts` verbatim, and the
 * results list is an L1 layer whose `→` teleports to L2 at the hit — seeding
 * the back stack the same way §4.4's mention teleport does [G15].
 */

import type { SearchHit } from "../client/types";
import type { Layer } from "../nav/layers";
import { MS_PER_DAY, MS_PER_SECOND } from "../time/units";
import { pad, truncateKeepingSuffix } from "../render/width";

/** A parsed query: the Slack operators plus the residual free text. */
export interface ParsedQuery {
  readonly from?: string;
  readonly in?: string;
  /** Unix ms, inclusive lower bound. */
  readonly after?: number;
  /** Unix ms, inclusive upper bound. */
  readonly before?: number;
  readonly text: string;
}

/** Operators, longest-first so `before:` is not eaten by a shorter prefix. */
const OPERATORS = ["before:", "after:", "from:", "in:"] as const;

/**
 * Parse Slack-style operators — `parseSearchOperators.ts` verbatim (§3.5).
 *
 * Two rules that look like details and are not:
 *
 * - **Operators must start at a token boundary**, deliberately not `\b`, so
 *   `built-in:react` and `https://x.com/in:foo` are not misparsed. A `\b` here
 *   would silently turn a URL into a channel filter and return nothing, which
 *   reads as "search is broken" rather than "your query was reinterpreted".
 * - **An invalid operator value stays in the FTS text** rather than erroring.
 *   `after:yesterday` should search for the words, not refuse the query.
 *
 * Date semantics, also verbatim: `after:` is local start-of-day **inclusive**;
 * `before:` is one second before local start-of-day, because NIP-01 `until` is
 * inclusive and Slack excludes the named day.
 */
export function parseQuery(query: string): ParsedQuery {
  const tokens = query.split(/\s+/).filter((t) => t.length > 0);
  const rest: string[] = [];
  let from: string | undefined;
  let inChannel: string | undefined;
  let after: number | undefined;
  let before: number | undefined;

  for (const token of tokens) {
    const operator = OPERATORS.find((op) => token.startsWith(op));
    if (!operator) {
      rest.push(token);
      continue;
    }
    const value = token.slice(operator.length);
    if (value.length === 0) {
      rest.push(token);
      continue;
    }
    switch (operator) {
      case "from:":
        from = value;
        break;
      case "in:":
        inChannel = value.startsWith("#") ? value.slice(1) : value;
        break;
      case "after:": {
        const parsed = parseDay(value);
        if (parsed === null) rest.push(token);
        else after = parsed;
        break;
      }
      case "before:": {
        const parsed = parseDay(value);
        if (parsed === null) rest.push(token);
        // One second before start-of-day: NIP-01 `until` is inclusive and Slack
        // excludes the named day, so the naive `parsed` would include it.
        else before = parsed - MS_PER_SECOND;
        break;
      }
    }
  }

  return {
    ...(from ? { from } : {}),
    ...(inChannel ? { in: inChannel } : {}),
    ...(after !== undefined ? { after } : {}),
    ...(before !== undefined ? { before } : {}),
    text: rest.join(" "),
  };
}

/** `YYYY-MM-DD` → start-of-day ms, or `null` when it is not a date. */
function parseDay(value: string): number | null {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return null;
  const ms = Date.parse(`${value}T00:00:00.000Z`);
  return Number.isNaN(ms) ? null : ms;
}

/**
 * The debounce for search-as-you-type (§3.5).
 *
 * > **Search-as-you-type, debounced 150 ms.** The debounce protects the relay's
 * > FTS, not just the render loop.
 */
export const SEARCH_DEBOUNCE_MS = 150;

/** Render the L1 RESULTS list. */
export function renderResults(
  hits: readonly SearchHit[],
  selected: number,
  cols: number,
  now: number,
): string[] {
  if (hits.length === 0) return [pad("  no results", cols)];
  const rows: string[] = [];
  hits.forEach((hit, index) => {
    const marker = index === selected ? "❯ " : "  ";
    rows.push(
      pad(
        `${marker}${truncateKeepingSuffix(
          `${hit.channelName} · ${hit.author}`,
          relativeDay(hit.ts, now),
          cols - 2,
        )}`,
        cols,
      ),
    );
    rows.push(
      pad(`  ${truncateKeepingSuffix(hit.excerpt, "", cols - 4)}`, cols),
    );
  });
  return rows;
}

/** `2026-07-28` for old hits, `14:09` for today's. */
function relativeDay(ts: number, now: number): string {
  const date = new Date(ts);
  if (Math.floor(now / MS_PER_DAY) === Math.floor(ts / MS_PER_DAY)) {
    return `${String(date.getUTCHours()).padStart(2, "0")}:${String(date.getUTCMinutes()).padStart(2, "0")}`;
  }
  return date.toISOString().slice(0, 10);
}

/**
 * Where `→`/`⏎` on a result teleports to — L2 at the hit (§1's map, §4.4).
 *
 * The crumb reads `results`, not `channels`, because the breadcrumb records the
 * path taken [G15]: `←` from here returns to the result list you were reading,
 * not to a channel list you never opened.
 */
export function teleportTarget(hit: SearchHit): Layer {
  return {
    kind: "channel",
    channelId: hit.channelId,
    eventId: hit.eventId,
    crumb: hit.channelName,
    selection: 0,
  };
}

/**
 * Filter a snapshot's messages locally — the fixture-backed search path.
 *
 * The real path is `GET /search`, where the daemon enforces the global
 * invariant that **no filter leaves without an explicit `kinds`** (§2.4): a
 * kindless filter can match a `P_GATED_KIND` and is refused with a 403. That
 * enforcement lives in the daemon precisely so the TUI structurally cannot get
 * it wrong, which is why nothing in this module names a kind.
 */
export function localSearch(
  messages: ReadonlyArray<{
    id: string;
    channelId: string;
    author: { name: string };
    ts: number;
    content: string;
  }>,
  channelNames: ReadonlyMap<string, string>,
  query: ParsedQuery,
): SearchHit[] {
  const needle = query.text.toLowerCase();
  return messages
    .filter((m) => {
      if (
        query.from &&
        m.author.name.toLowerCase() !== query.from.toLowerCase()
      )
        return false;
      if (query.in) {
        const name = (channelNames.get(m.channelId) ?? "")
          .replace("#", "")
          .toLowerCase();
        if (name !== query.in.toLowerCase()) return false;
      }
      if (query.after !== undefined && m.ts < query.after) return false;
      if (query.before !== undefined && m.ts > query.before) return false;
      if (needle.length > 0 && !m.content.toLowerCase().includes(needle))
        return false;
      return true;
    })
    .map((m) => ({
      eventId: m.id,
      channelId: m.channelId,
      channelName: channelNames.get(m.channelId) ?? m.channelId,
      author: m.author.name,
      ts: m.ts,
      excerpt: m.content,
    }))
    .sort((a, b) => b.ts - a.ts);
}
