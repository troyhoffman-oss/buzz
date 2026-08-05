/**
 * Where colour is spent — DESIGN.md §3.10's discipline, written down as code.
 *
 * §3.10 states the rule and names it the whole game:
 *
 * > **Neutral tones for large surfaces and chrome; high-chroma accents reserved
 * > for focus and selection.** This one rule is most of the difference between
 * > a TUI that looks designed and one that looks like a Christmas tree.
 *
 * The research on Claude Code (`nav/cc-research.md`) makes the same point from
 * the other side, and more strongly than expected: **not one of its fifteen
 * captured frames differentiates anything by colour.** Every distinction it
 * draws — object class, nesting, focus, status — is carried by glyph,
 * indentation, column alignment, or separator choice. Its one documented
 * intensity rule is a prohibition: "No pane borders, no dimmed-vs-bright pane
 * pairs."
 *
 * So the target is not "Buzz, but colourful". It is a frame that reads as
 * neutral text, in which the eye is pulled to exactly the things that carry
 * meaning. This module is the closed list of those things, and every renderer
 * imports from here rather than naming tokens inline — which is what stops the
 * list growing by one plausible-looking exception at a time until everything is
 * coloured and nothing is emphasised.
 *
 * # The four jobs colour is allowed to do
 *
 * 1. **Recede** — chrome that must be present but not read: rules, section
 *    headers, timestamps, placeholders, key hints. `textMuted` / `borderSubtle`.
 * 2. **Mark focus** — the single `❯` and the row it sits on. `accent`.
 * 3. **Carry state** — connection, agent working, errors, unread and mentions.
 *    `success` / `warning` / `error` / `primary`, and *only* on the token that
 *    is actually reporting state.
 * 4. **Separate speakers** — an agent's name from a human's, which is the one
 *    content-level distinction a chat client genuinely needs at a glance.
 *
 * Anything not on that list renders as base `text`, which is the default and
 * needs no token at all.
 */

import type { SpanStyle } from "./span";

/** Chrome that must be present but not read: rules, box edges, dividers. */
export const CHROME: SpanStyle = { fg: "borderSubtle" };

/**
 * Section headers — `COMMUNITY`, `PLACES`, `AGENTS`.
 *
 * They **recede**, they do not shout. Uppercase already does the structural
 * work (DESIGN's mocks use caps for every section label); adding colour on top
 * of caps would make the scaffolding louder than the content it organises,
 * which is the specific complaint that opened this pass.
 */
export const SECTION: SpanStyle = { fg: "textMuted" };

/** Timestamps, counts-without-attention, and other secondary metadata. */
export const META: SpanStyle = { fg: "textMuted" };

/** Composer placeholder and other "type here" prompts — present, not loud. */
export const PLACEHOLDER: SpanStyle = { fg: "textMuted" };

/** Key hints in a footer. Discoverable, never competing with content. */
export const HINT: SpanStyle = { fg: "textMuted" };

/** The one `❯` on screen [G8]. The single highest-chroma mark in the frame. */
export const FOCUS: SpanStyle = { fg: "accent", bold: true };

/**
 * The selected row's background.
 *
 * The owner's complaint was that selection is hard to see, and a marker glyph
 * alone is why: `❯` is two columns at the far left of a 120-column row, so on a
 * wide terminal the eye has to *find* it. A filled row is seen without being
 * looked for.
 *
 * `backgroundElement` rather than `accent` as the fill: an accent-filled row
 * inverts to `selectedListItemText` and becomes the loudest thing on screen by
 * a wide margin, which fights every other signal in the frame. A raised surface
 * reads as "this row", and the accent stays on the glyph where it means "keys
 * go here". Every text token clears AA against `backgroundElement` — the theme
 * suite asserts exactly that pair.
 */
export const SELECTED: SpanStyle = { bg: "backgroundElement" };

/** The dim `▌` a demoted row carries when the composer holds `❯` (§2.2). */
export const POSITION: SpanStyle = { fg: "textMuted" };

/** Unread volume — informational attention, not urgent. */
export const UNREAD: SpanStyle = { fg: "primary" };

/** Mentions and anything else addressed to *you*. The loudest state token. */
export const MENTION: SpanStyle = { fg: "warning", bold: true };

/** A live/working state: an agent mid-turn, a healthy connection. */
export const LIVE: SpanStyle = { fg: "success" };

/** A transient degradation: reconnecting, rate-limited, waking. */
export const DEGRADED: SpanStyle = { fg: "warning" };

/** A hard failure: auth rejected, endpoint missing, send refused. */
export const FAILED: SpanStyle = { fg: "error" };

/** A thread's reply counts — the thing §3's list-row rule refuses to drop. */
export const THREAD: SpanStyle = { fg: "info" };

/** A human author's name in the timeline. */
export const AUTHOR: SpanStyle = { fg: "text", bold: true };

/**
 * An agent author's name.
 *
 * The one content-level colour distinction this design spends, and it earns its
 * place: knowing at a glance whether the last twelve rows came from a person or
 * from a fleet member is the question a Buzz operator asks most often, and it is
 * the distinction the desktop app makes with an avatar that a terminal has no
 * room for.
 */
export const AGENT: SpanStyle = { fg: "secondary", bold: true };

/** Non-conversational system rows — "their own dimmed rows" (§3.1). */
export const SYSTEM: SpanStyle = { fg: "textMuted", italic: true };

/** The unread divider's label. The single most-tested piece of chat chrome. */
export const UNREAD_DIVIDER: SpanStyle = { fg: "primary", bold: true };

/** An unshipped destination's `Wave 2` tag (§3.7) — visible, plainly inert. */
export const WAVE_TAG: SpanStyle = { fg: "textMuted", italic: true };

/** The filled portion of the attention meter. */
export const METER_FILL: SpanStyle = { fg: "primary" };

/** The unfilled portion of the attention meter. */
export const METER_EMPTY: SpanStyle = { fg: "borderSubtle" };

/** Diff additions and removals, on their own rows (§3.1). */
export const DIFF_ADDED: SpanStyle = { fg: "diffAdded" };
export const DIFF_REMOVED: SpanStyle = { fg: "diffRemoved" };
export const DIFF_HEADER: SpanStyle = { fg: "diffHeader" };
