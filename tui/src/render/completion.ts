/**
 * The completion band — NAVIGATION.md §2.5, DESIGN.md §3.3 (retained verbatim).
 *
 * > `/`, `@`, `#`, `:`, and `ctrl+f` render **above the top rule**, leaving the
 * > composer and statusline fully intact and live. Flat, one level, `Esc`
 * > closes, arrows pick, `⏎`/`Tab` accepts. **This class augments what you are
 * > typing; the drawer inspects what is running. Never merge them.**
 *
 * §7 keeps §3.3 unchanged — "It is the completion band (§2.5); mechanics stay
 * verbatim" — so the trigger detection, ranking, and insertion rules below are
 * ux-patterns P11/P12 as the desktop implements them.
 */

import type { MentionCandidate } from "../client/types";
import type { CompletionKind } from "../app/state";
import { graphemes, pad, truncateKeepingSuffix, wrapHints } from "./width";

/** A detected trigger: which completion is open and what has been typed. */
export interface Trigger {
  readonly kind: CompletionKind;
  /** Grapheme offset of the trigger character. */
  readonly at: number;
  /** Text between the trigger and the cursor. */
  readonly query: string;
  /** True for `@@`, the agents-only trigger (§3.3). */
  readonly agentsOnly: boolean;
}

/** Trigger characters and the completion each opens. */
const TRIGGERS: ReadonlyArray<readonly [string, CompletionKind]> = [
  ["@", "mention"],
  ["#", "channel"],
  [":", "emoji"],
  ["/", "slash"],
];

/**
 * Detect an open trigger — **a pure function of (text, cursor offset)**, with
 * ux-patterns P11's three rules (§3.3):
 *
 * 1. nearest trigger backwards from the cursor;
 * 2. preceded by start-of-input or whitespace, **so `foo@bar` does not
 *    trigger**;
 * 3. no whitespace between the trigger and the cursor.
 *
 * All offsets are **display positions over grapheme clusters**, never byte or
 * JS-string indices — §3.3 is explicit, and getting it wrong misplaces the
 * insertion range the moment anyone types an emoji before a mention.
 *
 * `/` additionally requires column 0 (§1.4: "`/` command completion at composer
 * col 0"), so a URL's slashes never open the command list mid-sentence.
 */
export function detectTrigger(text: string, cursor: number): Trigger | null {
  const clusters = graphemes(text);
  const upto = clusters.slice(0, cursor);

  for (let i = upto.length - 1; i >= 0; i--) {
    const cluster = upto[i];
    if (cluster === undefined) continue;
    if (/\s/.test(cluster)) return null; // rule 3

    const match = TRIGGERS.find(([ch]) => ch === cluster);
    if (!match) continue;
    const [ch, kind] = match;

    const before = i > 0 ? upto[i - 1] : undefined;
    const atBoundary = before === undefined || /\s/.test(before);

    if (kind === "slash") {
      // Column 0 only. Anywhere else a `/` is a path separator or a URL.
      if (i !== 0) return null;
    } else if (!atBoundary) {
      // rule 2 — `foo@bar` is an email, not a mention.
      if (ch === "@" && before === "@") {
        // …except `@@`, the agents-only trigger (§3.3).
        return {
          kind: "mention",
          at: i - 1,
          query: upto.slice(i + 1).join(""),
          agentsOnly: true,
        };
      }
      return null;
    }

    return {
      kind,
      at: i,
      query: upto.slice(i + 1).join(""),
      agentsOnly: false,
    };
  }
  return null;
}

/**
 * Rank mention candidates — ux-patterns P12 (§3.3).
 *
 * > exact-prefix match doubles the score, then multiply by `(1 + frecency)`
 * > where frecency is `frequency / (1 + ageInDays)`. Local entities (roster
 * > members) always outrank directory-search results — the merge order is
 * > `[...ranked_roster, ...directory]`.
 *
 * The ranking lives here, in the TUI, on purpose: [D-2] makes the *candidate*
 * carry its pubkey from the daemon, so per-front-end personalization can never
 * make the ranked pick and the resolved tag diverge.
 */
export function rankCandidates(
  candidates: readonly MentionCandidate[],
  query: string,
  agentsOnly: boolean,
): MentionCandidate[] {
  const q = query.toLowerCase();
  const scored = candidates
    .filter((c) => !agentsOnly || c.isAgent)
    .map((candidate) => {
      const handle = candidate.handle.toLowerCase();
      const display = candidate.displayName.toLowerCase();
      if (q.length > 0 && !handle.includes(q) && !display.includes(q))
        return null;
      let score = 1;
      if (handle.startsWith(q)) score *= 2;
      score *= 1 + (candidate.frecency ?? 0);
      return { candidate, score };
    })
    .filter(
      (e): e is { candidate: MentionCandidate; score: number } => e !== null,
    );

  // Ties break on handle, not on input order: §3.3 requires a deterministic
  // list ("you can never build muscle memory" otherwise), and frecency ties are
  // common among prefix-colliding agent names — `claude-1`, `claude-2`,
  // `claude-3` is the orchestrator's normal case.
  scored.sort(
    (a, b) =>
      b.score - a.score || a.candidate.handle.localeCompare(b.candidate.handle),
  );
  return scored.map((e) => e.candidate);
}

/** The picker's key hints. `⇥` is an alias of `⏎`, not a second verb (§3.3). */
export const MENTION_HINTS = [
  "⏎/⇥ insert",
  "alt+N pick",
  "↑/↓ move",
  "esc close",
] as const;

/**
 * Render the mention picker (§3.3's mock).
 *
 * Humans and agents are **one list, sectioned** — sections are visual only and
 * a single selection cursor runs through all of them [LOCKED]. Offline agents
 * are shown rather than hidden, because mentioning one is a deliberate act of
 * queuing work; they carry a last-seen so the queue is an informed choice.
 *
 * Each row carries a **stable numeric index** for `alt+1`…`alt+9` (§3.3): with
 * frecency reordering the list between sessions, `@c` + `alt+2` is four
 * keystrokes regardless of what the ranking did.
 */
export function renderMentionPicker(
  candidates: readonly MentionCandidate[],
  selected: number,
  cols: number,
): string[] {
  if (candidates.length === 0) {
    // §3.3: "Enter with zero candidates sends nothing and inserts nothing. It
    // is a no-op that keeps the popup open with a `no matches` footer."
    // Leaving this undefined risks a half-composed message sent by a reflexive
    // Enter, which is the worst available outcome.
    return [
      pad("  no matches", cols),
      ...wrapHints([...MENTION_HINTS], cols).map((h) => pad(`  ${h}`, cols)),
    ];
  }

  const rows: string[] = [];
  let lastSection: "PEOPLE" | "AGENTS" | null = null;
  candidates.forEach((candidate, index) => {
    const section = candidate.isAgent ? "AGENTS" : "PEOPLE";
    if (section !== lastSection) {
      rows.push(pad(`  ${section}`, cols));
      lastSection = section;
    }
    const marker = index === selected ? "▸" : " ";
    const number = index < 9 ? String(index + 1) : " ";
    const presence = candidate.isAgent ? ` ${presenceMark(candidate)}` : "";
    const label = `${marker}${number} @${candidate.handle}   ${candidate.displayName}${presence}`;
    rows.push(
      pad(
        `  ${truncateKeepingSuffix(label, candidate.detail ?? "", cols - 2)}`,
        cols,
      ),
    );
  });
  for (const hint of wrapHints([...MENTION_HINTS], cols))
    rows.push(pad(`  ${hint}`, cols));
  return rows;
}

function presenceMark(candidate: MentionCandidate): string {
  switch (candidate.presence) {
    case "present":
      return "⬤";
    case "waking":
      return "◐ waking";
    case "offline":
      return "○ offline";
    case "unknown":
      return "◌";
  }
}

/**
 * Insert a completion — delete-range-then-insert (§3.3, P11g).
 *
 * > trailing space added only if the following char is not already a space.
 *
 * Returns the new text and cursor, both in grapheme offsets. Doing this over
 * clusters rather than JS string indices is what keeps a mention inserted after
 * an emoji from landing one position off.
 */
export function applyCompletion(
  text: string,
  trigger: Trigger,
  replacement: string,
): { text: string; cursor: number } {
  const clusters = graphemes(text);
  const cursorAt = trigger.at + 1 + graphemes(trigger.query).length;
  const after = clusters.slice(cursorAt);
  const needsSpace = after[0] !== " ";
  const inserted = `${replacement}${needsSpace ? " " : ""}`;
  const head = clusters.slice(0, trigger.at).join("");
  return {
    text: `${head}${inserted}${after.join("")}`,
    cursor: graphemes(`${head}${inserted}`).length,
  };
}

/**
 * The live mention counter — `n of 50` (§2.4 [D-2]).
 *
 * `MENTION_CAP` is a **build-time** rejection in the SDK, so it must be
 * surfaced **before** the send: "failing at Enter on a message the operator has
 * already written is the worst possible place to learn about a cap."
 */
export const MENTION_CAP = 50;

/** Whether adding one more mention would exceed the cap. */
export function mentionCapReached(count: number): boolean {
  return count >= MENTION_CAP;
}
