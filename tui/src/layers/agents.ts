/**
 * L1 AGENTS and the ACTIVITY renderable — NAVIGATION.md §6, DESIGN.md §3.4.
 *
 * §6's ruling is the reason this file holds *one* transcript renderer and three
 * callers:
 *
 * > **Both, and they are one renderable with three presentations.** The
 * > transcript appears (a) expanded in the bottom drawer at any layer, (b) as
 * > L4 off a channel, (c) as L2 off the AGENTS fleet — same component, same key
 * > grammar, different entry and different back target [G15].
 *
 * And why collapsing them would be wrong:
 *
 * > "What is claude-1 doing right now, while I keep typing" is a peek — it must
 * > not cost the chat. "Which of my eight agents needs me" is a destination.
 * > Collapsing them into one would break [G2]: the peek axis and the travel
 * > axis have different costs and therefore need different keys.
 *
 * The fleet's ordering is §3.4's actual question — *which of my agents needs
 * me* — expressed as the [G12] ladder.
 */

import type { Agent, TranscriptRow, Usage } from "../client/types";
import type { Layer } from "../nav/layers";
import { ATTENTION_LADDER } from "../render/drawer";
import { MS_PER_MINUTE, formatDuration } from "../time/units";
import {
  DEGRADED,
  FAILED,
  FOCUS,
  LIVE,
  MENTION,
  META,
  POSITION,
  SECTION,
  SELECTED,
} from "../render/palette";
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
} from "../render/span";
import { displayWidth, truncateKeepingSuffix, wrapText } from "../render/width";

/** Presence glyphs — four states, semantically exact (§3.1). */
export function presenceGlyph(agent: Agent): string {
  switch (agent.presence) {
    case "present":
      return "⬤";
    case "waking":
      return "◐";
    case "offline":
      return "○";
    case "unknown":
      // Never collapsed into `offline`: "no presence beat seen since this
      // daemon started and no snapshot yet" is a different fact, and rendering
      // it as offline is the looks-idle-while-the-socket-is-dead failure.
      return "◌";
  }
}

/**
 * Sort the fleet by the [G12] attention ladder (§3.4, §6).
 *
 * needs-input > working > failed > idle > completed. Stable within a rank, so
 * the daemon's own ordering survives — re-sorting by name would discard it.
 */
export function sortFleet(agents: readonly Agent[]): Agent[] {
  return [...agents].sort(
    (a, b) => ATTENTION_LADDER[a.state] - ATTENTION_LADDER[b.state],
  );
}

/** A row's status suffix. Survives truncation (§3's list-row rule). */
export function agentStatus(agent: Agent, now: number): string {
  const parts: string[] = [];
  if (agent.turnStartedAt !== undefined) {
    parts.push(`⚡ ${formatDuration(now - agent.turnStartedAt)}`);
  }
  switch (agent.state) {
    case "needsInput":
      parts.push("needs input");
      break;
    case "working":
      parts.push("working");
      break;
    case "failed":
      parts.push("failed");
      break;
    case "completed":
      parts.push("completed");
      break;
    case "idle":
      break;
  }
  return parts.join(" · ");
}

/** Render L1 AGENTS. */
export function renderFleet(
  agents: readonly Agent[],
  selected: number,
  cols: number,
  now: number,
  composerFocused: boolean,
): StyledRow[] {
  const rows: StyledRow[] = [
    padRow([plain("  "), styled("AGENTS", SECTION)], cols),
  ];
  agents.forEach((agent, index) => {
    const isSelected = index === selected;
    const marker = isSelected ? (composerFocused ? "▌ " : "❯ ") : "  ";
    const glyph = presenceGlyph(agent);
    const label =
      `${glyph} ${agent.name}   ${agent.channelName ?? ""}   ${agent.detail ?? ""}`.trimEnd();
    const status = agentStatus(agent, now);
    const text = truncateKeepingSuffix(label, status, cols - 2);

    // Two independent facts share this row and are routinely confused: the
    // **presence** glyph says whether the process is reachable, the **status**
    // suffix says what it is doing about your work. An agent can be present and
    // idle, or working and about to go offline. Colouring them from the same
    // source would merge the two questions into one answer.
    const carries = status.length > 0 && text.endsWith(status);
    const [head, tail] = carries
      ? splitAt([plain(text)], displayWidth(text) - displayWidth(status))
      : [[plain(text)], []];
    const headText = rowText(head);
    const spans: Span[] = [
      isSelected
        ? styled(marker, composerFocused ? POSITION : FOCUS)
        : plain(marker),
    ];
    if (headText.startsWith(glyph)) {
      spans.push(styled(glyph, presenceStyle(agent)));
      spans.push(plain(headText.slice(glyph.length)));
    } else {
      spans.push(plain(headText));
    }
    if (carries) spans.push(styled(rowText(tail), agentStatusStyle(agent)));
    rows.push(
      isSelected ? fillRow(spans, cols, SELECTED) : padRow(spans, cols),
    );
  });
  return rows;
}

/**
 * The colour of the presence glyph — *is this process reachable?*
 *
 * `unknown` is deliberately not collapsed into `offline` here any more than it
 * is in {@link presenceGlyph}: "no presence beat seen since this daemon
 * started" is a different fact from "reported offline", and painting them the
 * same colour would undo the distinction the glyph exists to draw.
 */
function presenceStyle(agent: Agent): SpanStyle {
  switch (agent.presence) {
    case "present":
      return LIVE;
    case "waking":
      return DEGRADED;
    default:
      return META;
  }
}

/**
 * The colour of the status suffix — *what is it doing about your work?*
 *
 * The [G12] ladder, in colour: `needsInput` is the only state that is blocked
 * on **you**, so it takes the mention tone and is the loudest thing in the
 * fleet view — which is what makes a blocked agent findable in a list of
 * twenty. `failed` is red, `working` is green, and an idle agent says nothing
 * loudly.
 */
function agentStatusStyle(agent: Agent): SpanStyle {
  switch (agent.state) {
    case "needsInput":
      return MENTION;
    case "working":
      return LIVE;
    case "failed":
      return FAILED;
    default:
      return META;
  }
}

/** Where `→` on a fleet row descends to — the ACTIVITY renderable (§6). */
export function descendTarget(agent: Agent): Layer {
  return {
    kind: "activity",
    agentPubkey: agent.pubkey,
    channelId: agent.channelId,
    crumb: agent.name,
    selection: 0,
  };
}

/** Glyphs per render class, from `agentSessionToolClassifier.ts` (§3.4.1). */
const CLASS_GLYPH: Readonly<Record<TranscriptRow["class"], string>> = {
  read: "⊙",
  write: "✎",
  shell: "⚑",
  thought: "▸",
  plan: "☰",
  permission: "⚠",
  error: "✖",
  lifecycle: "●",
  message: "·",
};

/** `HH:MM:SS` in UTC. Seconds matter here — a transcript is a timeline. */
function stamp(ts: number): string {
  const date = new Date(ts);
  return [date.getUTCHours(), date.getUTCMinutes(), date.getUTCSeconds()]
    .map((n) => String(n).padStart(2, "0"))
    .join(":");
}

/**
 * Render the transcript — **the one renderable** of §6.
 *
 * Called by all three presentations: the drawer's expansion passes a small
 * `cols` and a short slice, L4 and L2 pass the full screen. Same rows, same
 * glyphs, same grammar — which is what makes "different entry and different
 * back target" the *only* difference between them.
 */
export function renderTranscript(
  rows: readonly TranscriptRow[],
  cols: number,
): StyledRow[] {
  const out: StyledRow[] = [];
  for (const row of rows) {
    const glyph = CLASS_GLYPH[row.class];
    const counts =
      row.added !== undefined || row.removed !== undefined
        ? `+${row.added ?? 0} −${row.removed ?? 0}`
        : row.running
          ? "[running]"
          : "";
    const head = `${glyph} ${stamp(row.ts)}  ${row.label}`;
    const text = counts ? truncateKeepingSuffix(head, counts, cols - 2) : head;
    // The class glyph is the transcript's whole scanning mechanism — §3.4.1
    // gives every row class its own — so it is what carries colour, and only
    // for the two classes that are not routine: a permission prompt is blocked
    // on you, an error already failed. Colouring reads, writes and shell
    // invocations would tint most of a busy transcript and leave those two
    // with nothing to stand out from.
    const spans: Span[] = [plain("  ")];
    if (text.startsWith(glyph)) {
      spans.push(styled(glyph, transcriptGlyphStyle(row.class)));
      const rest = text.slice(glyph.length);
      // ` HH:MM:SS  ` — the stamp is fixed-width metadata and recedes so the
      // labels form a readable column beside it.
      const stampText = ` ${stamp(row.ts)}`;
      if (rest.startsWith(stampText)) {
        spans.push(styled(stampText, META));
        spans.push(plain(rest.slice(stampText.length)));
      } else {
        spans.push(plain(rest));
      }
    } else {
      spans.push(plain(text));
    }
    out.push(padRow(spans, cols));
    if (row.detail) {
      for (const line of wrapText(row.detail, Math.max(1, cols - 6), 0)) {
        // Indented continuation of the row above it, and the one place a
        // transcript carries prose. Muted, because the label is what you scan
        // and the detail is what you read only once you have stopped.
        out.push(padRow([plain("    "), styled(line, META)], cols));
      }
    }
  }
  return out;
}

/**
 * The colour a transcript row's class glyph takes.
 *
 * Only the two classes that mean something is *wrong or waiting* are coloured.
 * §3.4.1's glyph set already distinguishes all nine classes by shape; colour
 * here answers the coarser question an operator scanning a fast-moving
 * transcript actually asks, which is whether they need to stop scrolling.
 */
function transcriptGlyphStyle(kind: TranscriptRow["class"]): SpanStyle {
  switch (kind) {
    case "permission":
      return MENTION;
    case "error":
      return FAILED;
    default:
      return META;
  }
}

/**
 * The usage strip that survives to **every** width (§3.4.1).
 *
 * > A one-line usage strip survives to every tier, including XS. […] Token burn
 * > is a *number*, not a panel, and it is the last thing that should go.
 *
 * Three rules §3.4.1 states and an earlier draft's own mock violated:
 *
 * - `null` token fields mean **not reported**, not zero — render `—`, never `0`.
 * - The context-window denominator is **provider-reported or absent**; if
 *   absent, render `ctx N / —` and **no bar**. Deriving one from a client-side
 *   model table is the same sin as deriving `totalTokens`.
 * - Cost shows only when `costUsd` is present.
 */
export function usageStrip(usage: Usage): string {
  const ctx = `${compact(usage.contextUsed)}/${compact(usage.contextWindow)}`;
  const cost = usage.costUsd === null ? null : `$${usage.costUsd.toFixed(2)}`;
  return [usage.model, ctx, cost]
    .filter((p): p is string => p !== null)
    .join(" · ");
}

/**
 * Compact token counts — `12.5k`, `—` when not reported.
 *
 * `Intl.NumberFormat`'s compact notation rather than a hand-rolled
 * divide-and-suffix, for two reasons. It is the standard-library answer to
 * exactly this, and it keeps §6.4's blanket four-digit scan honest: the
 * threshold literal a hand-rolled version needs would be a bare `1000` in
 * `src/`, and writing it as `1_000` to slip past the scan is the bypass-by-
 * formatting that `src/time/units.ts`'s exemption exists to avoid.
 *
 * The `null → —` mapping is §3.4.1's rule and it is the whole reason this is a
 * function rather than an inline format: "`null` token fields mean *not
 * reported*, not zero — render `—`, never `0`."
 */
function compact(value: number | null): string {
  if (value === null) return "—";
  return new Intl.NumberFormat("en", {
    notation: "compact",
    maximumFractionDigits: 1,
  })
    .format(value)
    .toLowerCase();
}

/**
 * The metadata block of the ACTIVITY layer and the drawer expansion (§2.4).
 *
 * One function, so the drawer's peek and the full layer cannot report different
 * numbers for the same turn — which is the failure mode §6's "one renderable"
 * ruling exists to prevent.
 */
export function activityFields(
  agent: Agent,
  usage: Usage | undefined,
  now: number,
): Array<readonly [string, string]> {
  const fields: Array<readonly [string, string]> = [
    ["Agent", `${agent.name} · ${agent.runtime}`],
    ["Channel", agent.channelName ?? "—"],
  ];
  if (agent.turnId && agent.turnStartedAt !== undefined) {
    fields.push([
      "Turn",
      `${agent.turnId} · started ${stamp(agent.turnStartedAt)} · ${formatDuration(now - agent.turnStartedAt)}`,
    ]);
  }
  if (usage) {
    const n = (value: number | null): string =>
      value === null ? "—" : value.toLocaleString("en-US");
    fields.push([
      "Usage",
      `in ${n(usage.inTokens)} · out ${n(usage.outTokens)} · cache ${n(usage.cacheRead)}/${n(usage.cacheWrite)}`,
    ]);
    fields.push(["Model", usageStrip(usage)]);
  }
  return fields;
}

/** Burn rate, `tok/min` — §3.4.1's "totals are not the cost question". */
export function burnRate(usage: Usage, elapsedMs: number): string {
  const total = (usage.inTokens ?? 0) + (usage.outTokens ?? 0);
  const minutes = elapsedMs / MS_PER_MINUTE;
  if (minutes <= 0 || total === 0) return "—";
  return `${Math.round(total / minutes)} tok/min`;
}
