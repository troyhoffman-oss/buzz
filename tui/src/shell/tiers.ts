/**
 * The terminal floor — what survives of DESIGN.md §3.9 after NAVIGATION.md §7.
 *
 * §7 supersedes §3.9 in full:
 *
 * > **§3.9 Responsive tiers** — superseded. **No tier system.** Adaptive reflow
 * > per [G10]: list rows truncate keeping status, very narrow rows split to two
 * > lines, detail fields wrap with hanging indent, key hints wrap and never
 * > truncate, statusline rows truncate and never wrap. Phone is honest terminal
 * > width, not a tier.
 *
 * So the breakpoint table, the four-region layout it selected, and the
 * status-segment drop order that depended on it are all gone — replaced by
 * `src/render/width.ts`, whose functions take `cols` and *are* the reflow
 * rules. Every screen renders identically at every width; only the cuts change.
 * That is the property the T1 matrix asserts, by snapshotting each screen at 120
 * and 60 columns and comparing content rather than layout.
 *
 * Deleting the table rather than leaving it unused is deliberate. A superseded
 * mechanism that still compiles is a mechanism the next author will wire back
 * in — and a single `tierFor(cols)` call inside a screen would reintroduce
 * exactly the side-by-side layout §1 says does not survive the port.
 *
 * Two things from §3.9 survive, because §7 replaced the tiers and not the floor:
 *
 * 1. **The minimum size**, below which the app renders one legible line and
 *    **keeps running** — it does not exit and does not panic. Rendering into a
 *    zero-size rect is a no-op, never a crash.
 * 2. **The reason the floor is 40×16 and not 80×24.** An earlier draft declared
 *    80×24 while also making the phone a Wave-1 requirement — i.e. declared the
 *    phone both required and unsupported. §1.2 settles it: the phone is one of
 *    the two primary reading surfaces, so its width is a supported width. §7
 *    goes further and removes the concept of a phone *tier* entirely: it is
 *    honest terminal width, and the reflow rules are what make it legible.
 */

/**
 * Minimum supported terminal width.
 *
 * Not a tier boundary — there are no tiers. It is the width below which the
 * reflow rules stop being able to produce a legible row at all: 40 columns is
 * about `❯ ` plus a truncated label plus a status suffix, and the suffix is the
 * thing §3's list-row rule refuses to drop.
 */
export const MIN_COLS = 40;

/**
 * Minimum supported terminal height.
 *
 * 16 rows is the statusline's fixed 3 (§2.1) plus the two rules plus the
 * composer plus a body that can still show a message and its thread count.
 * §2.4's `min(18, floor(H * 0.5))` expansion cap is what keeps the drawer from
 * evicting that body at this height — the measured Claude Code failure was a
 * 20-row detail view on a 16-row terminal.
 */
export const MIN_ROWS = 16;

/** Whether the terminal is below the supported floor. */
export function isBelowFloor(cols: number, rows: number): boolean {
  return cols < MIN_COLS || rows < MIN_ROWS;
}
