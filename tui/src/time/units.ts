/**
 * Time units — the **only** module in `src/` allowed to hold a four-digit
 * literal, and the reason is DESIGN.md §6.4's boundary gate.
 *
 * `just tui-check-boundary` fails the build on any bare integer of four digits
 * or more anywhere under `src/`, because event kinds are daemon vocabulary and
 * a narrowed "only `kind`-adjacent numbers" rule already produced one silent
 * leak (`OBSERVER_FRAME = 24200` sailed through it). The gate's own comment
 * anticipates the honest escape:
 *
 * > a genuine non-kind constant of that size is rare in a front end and can be
 * > allowlisted explicitly below, whereas a missed kind is silent.
 *
 * So: one file, allowlisted by exact path in `scripts/check-boundary.sh`,
 * containing nothing but time constants. Every other module imports from here.
 * The alternative — writing `1_000` at the call site to slip past the digit
 * scan — would make the gate bypassable by formatting, in an undocumented way
 * the next author would find and copy. This mirrors what `themes/*.json` does
 * for the no-hex-in-`src/` rule: keep the rule absolute, give the legitimate
 * values one auditable home.
 */

/** Milliseconds in one second. */
export const MS_PER_SECOND = 1000;

/** Milliseconds in one minute. */
export const MS_PER_MINUTE = 60 * MS_PER_SECOND;

/** Milliseconds in one hour. */
export const MS_PER_HOUR = 60 * MS_PER_MINUTE;

/** Milliseconds in one day. */
export const MS_PER_DAY = 24 * MS_PER_HOUR;

/** Whole seconds in a millisecond duration, rounded up. */
export function toSeconds(ms: number): number {
  return Math.ceil(ms / MS_PER_SECOND);
}

/**
 * A duration as `MM:SS`, or `H:MM:SS` past an hour.
 *
 * Used by the turn badge (`turn 4a91 · 04:12`) and the drawer's agent rows. The
 * hour form appears rather than a wrapping `MM:SS` because a long-running turn
 * is exactly the case an operator is watching for, and `124:07` reads as noise.
 */
export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / MS_PER_SECOND));
  const seconds = total % 60;
  const minutes = Math.floor(total / 60) % 60;
  const hours = Math.floor(total / 3600);
  const mm = String(minutes).padStart(2, "0");
  const ss = String(seconds).padStart(2, "0");
  return hours > 0 ? `${hours}:${mm}:${ss}` : `${mm}:${ss}`;
}
