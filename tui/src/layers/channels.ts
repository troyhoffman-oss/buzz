/**
 * L1 CHANNELS — NAVIGATION.md §1, §8 ruling 1.
 *
 * > 1. **L1 composer: jump/filter only.** Typing at the channel list
 * >    fuzzy-filters; Enter enters the highlighted channel. No post-in-place.
 *
 * That ruling is what makes this layer's composer a *filter* rather than a
 * second send path, and it changes the focus rule's consequence: at L1 the list
 * owns `❯` while the composer is empty, and one typed character moves `❯` to
 * the composer while the list keeps filtering underneath (§2.2). So typing is
 * never ambiguous — it always narrows, and `⏎` always enters.
 *
 * Sections come from IA §3 via §1.3: starred · sections · Channels · Forums ·
 * DMs, with `⇧↑/⇧↓` moving between them (§1.1's structural jump).
 */

import type { Channel } from "../client/types";
import type { Layer } from "../nav/layers";
import {
  FOCUS,
  LIVE,
  MENTION,
  META,
  POSITION,
  SECTION,
  SELECTED,
  UNREAD,
} from "../render/palette";
import {
  type Span,
  type StyledRow,
  fillRow,
  padRow,
  plain,
  rowText,
  splitAt,
  styled,
} from "../render/span";
import {
  alignRight,
  displayWidth,
  truncateKeepingSuffix,
} from "../render/width";

/** A section of the channel list (§1.3). */
export type ChannelSection = "STARRED" | "CHANNELS" | "DMS";

/** One row of the L1 list. Section headers are rows — `⇧↑/⇧↓` land on them. */
export type ChannelRow =
  | { kind: "section"; section: ChannelSection }
  | { kind: "channel"; channel: Channel; score: number };

/**
 * Fuzzy subsequence match with a score — the filter half of §8 ruling 1.
 *
 * Subsequence rather than substring, because `eng` should find `#engineering`
 * *and* `#buzz-eng-notes`, and an operator typing three letters is describing a
 * shape rather than a prefix.
 *
 * Returns `null` for no match. The score rewards, in order:
 * - an exact prefix match (you typed the beginning of the name);
 * - matches at word boundaries (`bd` finds `#buzz-dev`);
 * - contiguous runs (a tight match beats a scattered one).
 *
 * **Deterministic**, and that is a requirement rather than a nicety. DESIGN
 * §3.3 makes the point for the mention picker and it applies here identically:
 * if the order can change between sessions you can never build muscle memory,
 * and every jump costs a visual confirmation. So the score is a pure function
 * of (query, name) with no recency term — unread and agent activity are shown
 * on the row, but they never reorder a filtered list.
 */
export function fuzzyScore(query: string, target: string): number | null {
  if (query.length === 0) return 0;
  const q = query.toLowerCase();
  const t = target.toLowerCase();

  let score = 0;
  let ti = 0;
  let previousMatch = -2;

  for (const ch of q) {
    const found = t.indexOf(ch, ti);
    if (found < 0) return null;
    if (found === previousMatch + 1) score += 8; // contiguous run
    const before = found > 0 ? t[found - 1] : undefined;
    if (
      before === undefined ||
      before === "-" ||
      before === "_" ||
      before === "#" ||
      before === " "
    ) {
      score += 6; // word boundary
    }
    // Earlier matches are worth more, so `#general` beats `#buzz-general` on `g`.
    score += Math.max(0, 10 - found);
    previousMatch = found;
    ti = found + 1;
  }

  if (t.startsWith(q) || t.startsWith(`#${q}`)) score += 40;
  return score;
}

/**
 * Build the L1 rows for a query.
 *
 * With an empty query the sections render in full, in the IA §3 order. With a
 * query the sections **collapse away**: a filtered list is a ranked list, and
 * keeping the section headers would mean the top match sits below two headers
 * you did not ask about. §1.1's structural jump has nothing to jump between at
 * that point, which is correct — there is no structure left, only rank.
 */
export function buildChannelRows(
  channels: readonly Channel[],
  query: string,
): ChannelRow[] {
  if (query.length > 0) {
    const scored = channels
      .map((channel) => ({ channel, score: fuzzyScore(query, channel.name) }))
      .filter(
        (entry): entry is { channel: Channel; score: number } =>
          entry.score !== null,
      )
      // Stable tiebreak on name, so two channels with equal scores never swap
      // between renders — the muscle-memory requirement again.
      .sort(
        (a, b) =>
          b.score - a.score || a.channel.name.localeCompare(b.channel.name),
      );
    return scored.map(({ channel, score }) => ({
      kind: "channel",
      channel,
      score,
    }));
  }

  const rows: ChannelRow[] = [];
  const starred = channels.filter((c) => c.starred);
  const plain = channels.filter((c) => !c.starred && c.kind !== "dm");
  const dms = channels.filter((c) => !c.starred && c.kind === "dm");

  if (starred.length > 0) {
    rows.push({ kind: "section", section: "STARRED" });
    for (const channel of starred)
      rows.push({ kind: "channel", channel, score: 0 });
  }
  if (plain.length > 0) {
    rows.push({ kind: "section", section: "CHANNELS" });
    for (const channel of plain)
      rows.push({ kind: "channel", channel, score: 0 });
  }
  if (dms.length > 0) {
    rows.push({ kind: "section", section: "DMS" });
    for (const channel of dms)
      rows.push({ kind: "channel", channel, score: 0 });
  }
  return rows;
}

/**
 * Whether a row can hold the selection.
 *
 * Every row can, matching home (`layers/home.ts`). Section headers are where
 * `⇧↑`/`⇧↓` land, and a jump target the cursor cannot occupy would leave no
 * visible `❯` between the jump and the following `↓` — breaking [G8] on the
 * exact sequence §4.1 draws. `⏎` on one collapses its section, so §5.1's "`⏎`
 * is never wrong" holds there too.
 */
export function isSelectable(_row: ChannelRow): boolean {
  return true;
}

/**
 * Whether a row is a *destination* — something `→` can descend into.
 *
 * Distinct from {@link isSelectable} on purpose: a section header is a place
 * the cursor can rest and a thing `⏎` can act on, but it is not a channel, and
 * `→` on it must do nothing rather than descend into an arbitrary member.
 */
export function isDestination(row: ChannelRow): boolean {
  return row.kind === "channel";
}

/**
 * §1.1's default selection, applied to the channel list.
 *
 * > the row you last left it from, else the first **attention-bearing** row
 * > (unread, mention, needs-input), else the first row.
 *
 * "Attention-bearing" here is mentions first, then unread, then an agent
 * working — the [G12] ladder's shape applied to a channel list, so entering
 * L1 lands you where something is waiting rather than at whatever sorts first.
 */
export function defaultChannelSelection(rows: readonly ChannelRow[]): number {
  const rank = (row: ChannelRow): number => {
    if (row.kind !== "channel") return 9;
    if (row.channel.mentions > 0) return 0;
    if (row.channel.agentsWorking.length > 0) return 1;
    if (row.channel.unread > 0) return 2;
    return 3;
  };
  let best = -1;
  let bestRank = 9;
  rows.forEach((row, i) => {
    const r = rank(row);
    if (r < bestRank) {
      bestRank = r;
      best = i;
    }
  });
  if (best >= 0 && bestRank < 3) return best;
  return rows.findIndex(isSelectable);
}

/** Structural-jump targets: the section headers (§1.1). */
export function structuralIndices(rows: readonly ChannelRow[]): number[] {
  const out: number[] = [];
  rows.forEach((row, i) => {
    if (row.kind === "section") out.push(i);
  });
  return out;
}

/** Where `→` on a channel row descends to. */
export function descendTarget(row: ChannelRow): Layer | null {
  if (row.kind !== "channel") return null;
  return {
    kind: "channel",
    channelId: row.channel.id,
    crumb: row.channel.name,
    selection: 0,
  };
}

/**
 * The ambient status a channel row carries — IA §5.3's signal, made reachable.
 *
 * > IA §5.3 establishes that "which agents are working where" is ambient on the
 * > *channel list*, not only inside an agent screen, and IA §6.4 records that
 * > the desktop gives that signal no keyboard reachability at all.
 *
 * Rendered as a **status suffix**, so it survives truncation alongside the
 * unread count (§3's list-row rule): at 60 columns the channel name ellipsizes
 * and `8 · ⚡claude-1` does not.
 */
export function channelStatus(channel: Channel): string {
  return channelStatusParts(channel)
    .map((part) => part.text)
    .join(" · ");
}

/**
 * The status suffix as its **parts**, each carrying its own meaning.
 *
 * Three different signals share this one slot — volume, address, activity — and
 * they are not equally urgent. `8` unread is information; `@2` is addressed to
 * you; `⚡claude-1` is something happening right now. Rendering them in one
 * colour makes the operator read all three to find the one they care about,
 * which is the opposite of what a scannable status column is for.
 *
 * Exposed as parts rather than styled by matching the joined string, for the
 * reason `render/span.ts` states: a channel whose *name* contains `@` would
 * defeat a matcher, and the renderer already knows which part is which.
 * {@link channelStatus} stays the single source of the text.
 */
export function channelStatusParts(channel: Channel): readonly Span[] {
  const parts: Span[] = [];
  if (channel.unread > 0) parts.push(styled(String(channel.unread), UNREAD));
  if (channel.mentions > 0) parts.push(styled(`@${channel.mentions}`, MENTION));
  if (channel.agentsWorking.length > 0) {
    parts.push(styled(`⚡${channel.agentsWorking.join(" ")}`, LIVE));
  }
  return parts;
}

/**
 * Render the L1 list.
 *
 * The status suffix is re-attributed span by span **after** truncation rather
 * than before it. `truncateKeepingSuffix` guarantees the whole suffix survives
 * (§3's list-row rule — the label ellipsizes, the counts do not), so its text
 * is present verbatim at the end of the finished row, and cutting the row at
 * the suffix's width recovers exactly the parts that went in. Building the row
 * from pre-styled pieces instead would mean reimplementing the truncation, and
 * the reimplementation is what drifts.
 */
export function renderChannels(
  rows: readonly ChannelRow[],
  selected: number,
  cols: number,
  composerFocused: boolean,
): StyledRow[] {
  return rows.map((row, index) => {
    const isSelected = index === selected;
    const marker = isSelected ? (composerFocused ? "▌ " : "❯ ") : "  ";
    // A section header is never the cursor's row *and* never carries a marker,
    // so it is built whole — and it recedes, because the caps already say it
    // is a header.
    if (row.kind === "section") {
      const body: StyledRow = [plain("  "), styled(row.section, SECTION)];
      return isSelected ? fillRow(body, cols, SELECTED) : padRow(body, cols);
    }

    const statusParts = channelStatusParts(row.channel);
    const status = channelStatus(row.channel);
    const label =
      row.channel.kind === "dm" ? `◍ ${row.channel.name}` : row.channel.name;
    const text = status
      ? truncateKeepingSuffix(label, status, cols - 2)
      : alignRight(label, "", cols - 2);

    const spans: Span[] = [
      isSelected
        ? styled(marker, composerFocused ? POSITION : FOCUS)
        : plain(marker),
    ];
    if (status && text.endsWith(status)) {
      const [head] = splitAt(
        [plain(text)],
        displayWidth(text) - displayWidth(status),
      );
      spans.push(plain(rowText(head)));
      // Re-emit the parts in order, with the ` · ` separators muted so the
      // three signals read as three rather than as one run.
      statusParts.forEach((part, i) => {
        if (i > 0) spans.push(styled(" · ", META));
        spans.push(part);
      });
    } else {
      // The suffix did not survive intact (a width below its own), so there is
      // nothing to attribute and the row is one truncated fragment.
      spans.push(plain(text));
    }
    return isSelected ? fillRow(spans, cols, SELECTED) : padRow(spans, cols);
  });
}
