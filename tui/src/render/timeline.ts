/**
 * Message rendering — DESIGN.md §3.1's load-bearing details, laid out per
 * NAVIGATION.md §7's reflow rules.
 *
 * §3.1's four rules, and why each is here rather than in the layer:
 *
 * - **Author grouping and day dividers** port from `messageGrouping.ts`.
 *   Consecutive messages by one author within the window collapse to one header.
 * - **The unread divider is anchored to an event id**, never a row index. "It
 *   must survive a reconnect burst, a re-render, and a tier change. This is the
 *   single most-tested piece of chat chrome."
 * - **Diff messages get their own row**, not an attachment chip — collapsed to
 *   a header plus the changed-hunk summary. "Terminals were built for this and
 *   it is the clearest 'better here than on the desktop' win in Wave 1."
 * - **Non-conversational kinds** render as their own dimmed rows.
 *
 * Chat is 95% of user time, so this module is where "chrome justifies every
 * row" is enforced: a grouped message costs one row, a day divider costs one,
 * and the unread divider costs one. Nothing else is spent.
 *
 * # Where colour is spent here, and where it deliberately is not
 *
 * The timeline is the largest surface in the product, which makes it the
 * surface DESIGN §3.10's rule is really about — "neutral tones for large
 * surfaces and chrome; high-chroma accents reserved for focus and selection".
 * So the split is: everything §3.1 calls *chrome* is attributed (the divider
 * rules, the author line, the diff box, the thread counts), and **message
 * content is never coloured at all**. A body row is base text, always.
 *
 * That asymmetry is the point. If bodies carried colour there would be nothing
 * left for the day divider and the `● new` anchor to stand out *against*, and
 * the M3 captures are the evidence: one flat weight across four hundred rows is
 * why the frames read as a wall rather than as a conversation. Colour here buys
 * back the structure the grouping rules already computed — you can see where a
 * day starts, where a speaker changes, whether that speaker is a person or a
 * fleet member, and where you stopped reading — without any of it competing
 * with the words.
 */

import type { Message } from "../client/types";
import { MS_PER_DAY, MS_PER_MINUTE } from "../time/units";
import {
  AGENT,
  AUTHOR,
  CHROME,
  DIFF_ADDED,
  DIFF_HEADER,
  DIFF_REMOVED,
  FOCUS,
  HINT,
  LIVE,
  META,
  SELECTED,
  SYSTEM,
  THREAD,
  UNREAD_DIVIDER,
} from "./palette";
import {
  type Span,
  type SpanStyle,
  type StyledRow,
  fillRow,
  padRow,
  plain,
  rowText,
  splitAt,
  styled,
} from "./span";
import {
  alignRight,
  displayWidth,
  truncateKeepingSuffix,
  wrapText,
} from "./width";

/**
 * The author-grouping window, from `messageGrouping.ts`.
 *
 * Five minutes: long enough that a burst of consecutive messages reads as one
 * turn, short enough that a reply twenty minutes later gets its own header and
 * timestamp.
 */
export const GROUPING_WINDOW_MS = 5 * MS_PER_MINUTE;

/**
 * One rendered row of the timeline, with what it came from.
 *
 * `text` holds a {@link StyledRow} rather than a `string`, and keeps the name:
 * every reader either paints it or projects it back with `rowText`, and a
 * second field name for "the same row, styled" would only invite the two to
 * drift. `rowText(row.text)` is byte-for-byte what this field used to hold —
 * `padRow` applies the same clip-and-fill `pad` did — so the row geometry the
 * T1 matrix and the reflow suite assert is unchanged by construction rather
 * than by re-verification.
 */
export interface TimelineRow {
  readonly text: StyledRow;
  /** The message this row belongs to; absent on dividers. */
  readonly messageId?: string;
  /** True when this is the row a message-select cursor should mark. */
  readonly selectable: boolean;
  readonly kind: "header" | "body" | "meta" | "divider" | "diff";
}

/** `HH:MM` in UTC. See `layers/home.ts` for why UTC and not local. */
function clock(ts: number): string {
  const date = new Date(ts);
  return `${String(date.getUTCHours()).padStart(2, "0")}:${String(date.getUTCMinutes()).padStart(2, "0")}`;
}

/** `Tue 4 Aug` in UTC, for the day divider. */
function dayLabel(ts: number): string {
  const date = new Date(ts);
  const days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const months = [
    "Jan",
    "Feb",
    "Mar",
    "Apr",
    "May",
    "Jun",
    "Jul",
    "Aug",
    "Sep",
    "Oct",
    "Nov",
    "Dec",
  ];
  return `${days[date.getUTCDay()]} ${date.getUTCDate()} ${months[date.getUTCMonth()]}`;
}

/** UTC day index, for day-divider comparison. */
function dayIndex(ts: number): number {
  return Math.floor(ts / MS_PER_DAY);
}

/**
 * Whether `message` continues `previous`'s group (`messageGrouping.ts`).
 *
 * A system row never groups: §3.1 gives non-conversational kinds "their own
 * dimmed rows", and folding one under a human's header would attribute it to
 * that human.
 */
export function continuesGroup(
  previous: Message | undefined,
  message: Message,
): boolean {
  if (!previous) return false;
  if (previous.system || message.system) return false;
  if (previous.author.pubkey !== message.author.pubkey) return false;
  if (dayIndex(previous.ts) !== dayIndex(message.ts)) return false;
  return message.ts - previous.ts <= GROUPING_WINDOW_MS;
}

/** A thread's status suffix: `⤷ 4 · 2 new`. Survives truncation (§3). */
export function threadSuffix(message: Message): string {
  if (!message.replyCount) return "";
  const newPart = message.unreadReplyCount
    ? ` · ${message.unreadReplyCount} new`
    : "";
  return `⤷ ${message.replyCount}${newPart}`;
}

/** A reaction strip: `♥2 💯1` (§3.1). */
function reactionStrip(message: Message): string | null {
  if (!message.reactions || message.reactions.length === 0) return null;
  return message.reactions.map((r) => `${r.emoji}${r.count}`).join("  ");
}

/**
 * The author header: `matt                                            14:09`.
 *
 * Three attributions on one row, and each answers a different question the eye
 * asks when it lands on a header: *who* (the name), *what kind of who* (agent
 * or human), and *when* (the stamp, which recedes because it is the thing you
 * read last). §3.1 spends a whole row on this every time the speaker changes,
 * and the M3 capture drew all three at one weight — so the row that exists to
 * announce a speaker change announced it no louder than the text below it.
 *
 * `alignRight` still produces the string. The spans are cut out of it by
 * display column rather than the layout being rebuilt in span form, for the
 * reason `render/span.ts` gives: two implementations of the same arithmetic
 * drift, and the one that drifts silently is the one nothing asserts. Cutting
 * the finished row means the text is `alignRight`'s by construction — a wrong
 * boundary here can only mis-colour, never mis-draw.
 */
function authorHeader(message: Message, cols: number): StyledRow {
  const presence = message.author.isAgent ? " ⬤" : "";
  const stamp = clock(message.ts);
  const cell = `${message.author.name}${presence}`;
  const text = `  ${alignRight(cell, stamp, cols - 2)}`;
  const nameStyle = message.author.isAgent ? AGENT : AUTHOR;

  // Below the stamp's own width `alignRight` abandons the label and clips the
  // stamp itself, so there is no name left to attribute and the whole row is
  // one metadata fragment.
  if (cols - 2 <= displayWidth(stamp)) return padRow([styled(text, META)], cols);

  // The stamp is `alignRight`'s right-hand segment, placed whole at the end,
  // and `HH:MM` is ASCII — so its code-unit length is also its column count.
  const label = text.slice(2, text.length - stamp.length);
  const fits = displayWidth(cell) <= cols - 2 - displayWidth(stamp) - 1;

  const spans: Span[] = [plain("  ")];
  if (fits && presence.length > 0) {
    // Untruncated, so the label is exactly `name + presence`: the glyph reports
    // *live*, which is a state and not part of the name, and colouring it with
    // the name would sink the one ambient signal a fleet operator scans for.
    spans.push(styled(message.author.name, nameStyle));
    spans.push(styled(presence, LIVE));
    spans.push(plain(label.slice(cell.length)));
  } else {
    // Truncated (or no glyph): the alignment gap rides the name's style, which
    // draws nothing — a foreground on spaces is invisible — and keeps the
    // ellipsis in the name's colour, where it belongs.
    spans.push(styled(label, nameStyle));
  }
  spans.push(styled(stamp, META));
  return padRow(spans, cols);
}

/**
 * Render a diff message as its own rows (§3.1).
 *
 * > **Diff messages get their own row**, not an attachment chip. Collapsed to a
 * > header plus the changed-hunk summary; `Enter` expands to full unified diff.
 *
 * Collapsed is the default because a 200-line diff in a chat timeline is a
 * timeline you have lost. The header carries the path and the `+18 −4` counts,
 * which is what you scan for; the hunks are the expansion.
 */
function renderDiff(
  message: Message,
  cols: number,
  expanded: boolean,
): TimelineRow[] {
  const diff = message.diff;
  if (!diff) return [];
  const counts = `+${diff.added} −${diff.removed}`;
  // The box glyphs are structure and recede; the path and its counts are what
  // you scan a diff row *for*, so they keep the header colour. Colouring the
  // whole card one way would make the frame it draws compete with the change
  // it contains — §3.10's chrome rule, applied at the smallest scale it has.
  const rows: TimelineRow[] = [
    {
      text: padRow(
        [
          plain("  "),
          styled("┌ diff · ", CHROME),
          styled(
            truncateKeepingSuffix(diff.path, counts, Math.max(1, cols - 12)),
            DIFF_HEADER,
          ),
        ],
        cols,
      ),
      messageId: message.id,
      selectable: false,
      kind: "diff",
    },
  ];
  if (expanded) {
    for (const hunk of diff.hunks) {
      const sign =
        hunk.kind === "add" ? "+" : hunk.kind === "remove" ? "-" : " ";
      const lineNo = String(hunk.newLine ?? hunk.oldLine ?? "").padStart(4);
      // The sign carries the colour, and it carries it over the *whole* hunk
      // line. A diff is the one place in this design where colouring content
      // is correct rather than noise: added and removed are not decoration on
      // the text, they are what the text means, and every diff viewer an
      // operator has ever used says so this way.
      const style: SpanStyle =
        hunk.kind === "add"
          ? DIFF_ADDED
          : hunk.kind === "remove"
            ? DIFF_REMOVED
            : {};
      rows.push({
        text: padRow(
          [
            plain("  "),
            styled("│ ", CHROME),
            styled(lineNo, META),
            styled(" │", CHROME),
            styled(`${sign}${hunk.text}`, style),
          ],
          cols,
        ),
        messageId: message.id,
        selectable: false,
        kind: "diff",
      });
    }
  } else {
    rows.push({
      text: padRow(
        [
          plain("  "),
          styled("│  ", CHROME),
          styled(`${diff.hunks.length} hunks`, META),
          styled(" · ⏎ expand · y yank", HINT),
        ],
        cols,
      ),
      messageId: message.id,
      selectable: false,
      kind: "diff",
    });
  }
  rows.push({
    text: padRow([plain("  "), styled("└", CHROME)], cols),
    messageId: message.id,
    selectable: false,
    kind: "diff",
  });
  return rows;
}

/** What the timeline renderer needs beyond the messages themselves. */
export interface TimelineOptions {
  readonly cols: number;
  /**
   * The event id the unread divider sits **after** — never a row index (§3.1).
   *
   * Anchoring to the id is what lets the divider survive a reconnect burst: new
   * messages arrive, rows shift, and the divider stays attached to the message
   * it describes. An index-anchored divider drifts on the first insert.
   */
  readonly unreadAfterEventId?: string;
  /** Message ids whose diffs are expanded. */
  readonly expandedDiffs?: ReadonlySet<string>;
  /** The message-select cursor's message id, when active (§3). */
  readonly selectedMessageId?: string;
}

/**
 * Render a channel timeline to rows.
 *
 * The **same function at every width** — §7's reflow, not tiers. At 60 columns
 * the body wraps and the thread counts survive; at 120 the same content fits on
 * fewer rows. Nothing is added or removed by width, which is the property the
 * T1 matrix asserts.
 */
export function renderTimeline(
  messages: readonly Message[],
  options: TimelineOptions,
): TimelineRow[] {
  const { cols } = options;
  const rows: TimelineRow[] = [];
  let previous: Message | undefined;

  for (const message of messages) {
    // Day divider, before anything else in the day.
    if (!previous || dayIndex(previous.ts) !== dayIndex(message.ts)) {
      const label = ` ${dayLabel(message.ts)} `;
      const bar = Math.max(0, cols - label.length - 2);
      // The rule recedes and the date does not. Drawn in one weight — as it
      // was — a hundred and eighteen dashes shout as loudly as the four
      // characters that carry the information, and the divider stops reading
      // as a label at all. This is the cheapest hierarchy in the whole
      // timeline and the M3 captures are what it was missing.
      rows.push({
        text: padRow(
          [
            plain("  "),
            styled("─".repeat(Math.floor(bar / 2)), CHROME),
            styled(label, META),
            styled("─".repeat(Math.ceil(bar / 2)), CHROME),
          ],
          cols,
        ),
        selectable: false,
        kind: "divider",
      });
    }

    if (message.system) {
      // Non-conversational rows get their own dimmed row and never group.
      rows.push({
        text: padRow(
          [
            plain("  "),
            styled(`⋯ ${clock(message.ts)}  ${message.content}`, SYSTEM),
          ],
          cols,
        ),
        messageId: message.id,
        selectable: true,
        kind: "meta",
      });
      previous = message;
      continue;
    }

    if (!continuesGroup(previous, message)) {
      rows.push({
        text: authorHeader(message, cols),
        messageId: message.id,
        selectable: false,
        kind: "header",
      });
    }

    if (message.diff) {
      rows.push(
        ...renderDiff(
          message,
          cols,
          options.expandedDiffs?.has(message.id) ?? false,
        ),
      );
    } else {
      // The message-select cursor marks the *body* row, so the `❯` sits on the
      // text you are selecting rather than on the author header above it.
      const selected = options.selectedMessageId === message.id;
      const marker = selected ? "❯ " : "  ";
      const suffix = threadSuffix(message);
      const wrapped = wrapText(message.content, Math.max(1, cols - 2), 0);
      wrapped.forEach((line, i) => {
        const isLast = i === wrapped.length - 1;
        const text =
          isLast && suffix
            ? truncateKeepingSuffix(line, suffix, cols - 2)
            : line;
        // The thread suffix keeps its own colour inside an otherwise neutral
        // body row: §3's list-row rule makes those counts the thing you pick a
        // conversation *by*, so they are the one part of a body that is not
        // content. Splitting by the suffix's width preserves the exact text
        // `truncateKeepingSuffix` produced, gap included.
        const carriesSuffix = isLast && suffix.length > 0 && text.endsWith(suffix);
        const [head, tail] = carriesSuffix
          ? splitAt([plain(text)], displayWidth(text) - displayWidth(suffix))
          : [[plain(text)], []];
        const body: StyledRow = [
          i === 0 ? styled(marker, selected ? FOCUS : {}) : plain("  "),
          plain(rowText(head)),
          ...(carriesSuffix ? [styled(rowText(tail), THREAD)] : []),
        ];
        rows.push({
          // A selected message must be seen without being looked for: `❯` is
          // two columns at the far left of a 120-column row, so on a wide
          // terminal the eye has to hunt for it. The fill is what answers the
          // owner's "selection is hard to see", and it is scoped to the body
          // row the cursor actually marks.
          text: selected
            ? fillRow(body, cols, SELECTED)
            : padRow(body, cols),
          messageId: message.id,
          selectable: i === 0,
          kind: "body",
        });
      });
      // A thread suffix that could not fit on the body's last row gets its own,
      // rather than being dropped: §3's list-row rule says the counts survive,
      // and a one-row cost is cheaper than an unpickable thread.
      if (suffix && wrapped.length > 0) {
        const last = rows.at(-1);
        // `rowText` rather than a raw `.includes` on the field: the row holds
        // spans now, and the question being asked is about the *text* it
        // draws — which is exactly what the projection is for.
        if (last && !rowText(last.text).includes(suffix)) {
          rows.push({
            text: padRow([plain("  "), styled(suffix, THREAD)], cols),
            messageId: message.id,
            selectable: false,
            kind: "meta",
          });
        }
      }
    }

    const reactions = reactionStrip(message);
    if (reactions) {
      rows.push({
        text: padRow([plain("  "), styled(reactions, META)], cols),
        messageId: message.id,
        selectable: false,
        kind: "meta",
      });
    }

    if (options.unreadAfterEventId === message.id) {
      const label = " ● new ";
      const bar = Math.max(0, cols - label.length - 2);
      // §3.1 calls this "the single most-tested piece of chat chrome", and it
      // is the one divider that is genuinely an *alert*: it marks where you
      // stopped reading. So unlike the day divider — whose label is muted
      // metadata — the label here carries the unread colour at full weight,
      // while the rule around it recedes like every other rule.
      rows.push({
        text: padRow(
          [
            plain("  "),
            styled("─".repeat(2), CHROME),
            styled(label, UNREAD_DIVIDER),
            styled("─".repeat(Math.max(0, bar)), CHROME),
          ],
          cols,
        ),
        selectable: false,
        kind: "divider",
      });
    }

    previous = message;
  }

  return rows;
}

/**
 * Message ids that are **thread roots** — `⇧↑`/`⇧↓`'s structural unit (§3).
 *
 * > `⇧↑`/`⇧↓` step **thread root to thread root** — skip everything with no
 * > replies.
 *
 * > In a channel with 400 messages and 6 live threads, six `⇧↑` presses reach
 * > the oldest live thread, and nothing else is ever selected on the way.
 *
 * A message with replies is a root even if it is itself a reply: what the
 * gesture is for is "pick a conversation", and a reply that grew its own
 * conversation is one.
 */
export function threadRootIds(messages: readonly Message[]): string[] {
  return messages.filter((m) => (m.replyCount ?? 0) > 0).map((m) => m.id);
}

/** Message ids in select order, newest last — the message-select axis (§3). */
export function selectableIds(messages: readonly Message[]): string[] {
  return messages.filter((m) => !m.replyTo).map((m) => m.id);
}
