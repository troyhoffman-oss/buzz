/**
 * Replay a fixture scenario's event stream — the half of §5.4 that was missing.
 *
 * §5.4 defines a scenario as "an initial state snapshot, **then the event
 * stream**", and {@link FixtureClient} implements both halves faithfully:
 * `advanceTo(ms)` delivers every frame due at or before an offset. But nothing
 * in the product ever called it. `main.ts` constructed the client and booted the
 * shell, and the only `advanceTo` caller in the repository was
 * `test/helpers/drive.ts` — so under `BUZZ_TUI_FIXTURE` the shipped binary
 * rendered line 1 of a scenario and then sat on it forever.
 *
 * That is worse than a missing feature, because it is invisible in exactly the
 * way §1.3 property 3 warns about. Every multi-line scenario *looked* fine: the
 * snapshot is the bulk of the file, so the frame is full, plausible, and wrong
 * only in what never arrives. It cost this lane a real artifact — the Wave 2
 * capture set records `agent-stream` and `reconnect` frames that are in truth
 * `seeded-basic` frames, and the reconnect capture was written off as
 * unlandable ("a window this harness cannot land in deterministically") when the
 * window it could not land in had never opened.
 *
 * # Why the driver lives here and not in `FixtureClient`
 *
 * The obvious repair is a `setInterval` inside the client. That would break the
 * property its header is built on and that T1 depends on:
 *
 * > Replay is driven by an explicit clock, not `setTimeout`. T1 snapshots must
 * > be deterministic (§5.3 requirement 1).
 *
 * A client that advanced itself would make every render-suite assertion a race
 * against a timer nobody asked for. So the clock stays explicit and *this*
 * module supplies the one caller that wants wall-clock pacing, which keeps the
 * timer out of the transport and out of every test that is not about timing.
 *
 * `tick()` is separated from the interval that calls it for the same reason: the
 * scheduling is one line in `main.ts` and the *arithmetic* is testable against a
 * fake clock with no timers at all.
 */

import type { FixtureClient } from "./fixture-client";

/** How a replay is paced and where, if anywhere, it stops. */
export interface ReplayOptions {
  /** Wall clock. Injectable so the tick arithmetic is testable without timers. */
  readonly now?: () => number;
  /**
   * Freeze the scenario at this offset in ms — `BUZZ_TUI_FIXTURE_UNTIL`.
   *
   * Without it a scenario plays to its end, which is right for dogfooding and
   * wrong for capturing a **transient** state: `reconnect` is degraded from 500
   * ms to 2000 ms, so a capture of it is a race the harness loses about as often
   * as it wins. Bounding the replay clock turns that window into a resting
   * state — the pane reaches `◌ retry 2` and stays there, so `settle()` (which
   * waits for the frame to stop changing) has something to settle on.
   *
   * This is the same move `BUZZ_TUI_FIXED_TIME` already makes for the render
   * clock, applied to the replay clock: freeze it, and a transient becomes
   * observable. §5.3 requirement 1 is the general form of the rule.
   */
  readonly until?: number;
}

/** A started replay. Call {@link Replay.tick} until it reports completion. */
export interface Replay {
  /**
   * Advance the scenario to the elapsed wall-clock offset.
   *
   * Returns `true` when nothing further will ever be delivered — the scenario
   * drained, or it reached its {@link ReplayOptions.until} bound — so the caller
   * can stop its interval rather than waking forever on a finished stream.
   */
  tick(): boolean;
}

/**
 * Begin replaying `client`'s scenario against the wall clock.
 *
 * Time zero is the moment this is called, so a scenario's `atMs` offsets mean
 * "this long after the TUI started", which is what an author writing `atMs: 500`
 * already assumes.
 */
export function startFixtureReplay(
  client: FixtureClient,
  options: ReplayOptions = {},
): Replay {
  const now = options.now ?? (() => Date.now());
  const { until } = options;
  const started = now();

  return {
    tick(): boolean {
      // `Math.max(0, …)` because a clock that steps backwards (NTP, a fake in a
      // test) must not rewind the cursor — `advanceTo` only ever moves forward,
      // and handing it a smaller offset would silently stall the stream rather
      // than error.
      const elapsed = Math.max(0, now() - started);
      const target = until === undefined ? elapsed : Math.min(elapsed, until);
      client.advanceTo(target);
      // Drained first: a scenario whose last frame is before the bound is
      // finished regardless of the bound, and reporting otherwise would keep an
      // interval alive for nothing.
      if (client.isDrained()) return true;
      return until !== undefined && target >= until;
    },
  };
}

/**
 * `BUZZ_TUI_FIXTURE_UNTIL` in ms, or `undefined` to play the scenario out.
 *
 * A malformed value is rejected rather than ignored. Ignoring it would let a
 * capture harness *believe* it had pinned a transient state while the scenario
 * ran past it — which is precisely the class of silent, plausible-looking wrong
 * artifact this module exists to stop producing.
 */
export function replayUntilFromEnv(
  env: Record<string, string | undefined>,
): number | undefined {
  const raw = env.BUZZ_TUI_FIXTURE_UNTIL;
  if (raw === undefined) return undefined;
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed < 0) {
    throw new Error(
      `BUZZ_TUI_FIXTURE_UNTIL is not a non-negative number of ms: ${raw}`,
    );
  }
  return parsed;
}

/** How often the replay clock is sampled. */
export const REPLAY_TICK_MS = 50;
