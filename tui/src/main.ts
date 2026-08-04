/**
 * `buzz-tui` entry point — DESIGN.md §2.3, §5.3, §5.4.
 *
 * **The user runs `buzz-tui`.** There is no "start the daemon first", no port
 * to pick, no config file to author before first use. The daemon is an
 * implementation detail that happens to survive Ctrl-C (§1.3 property 1).
 *
 * TODO(wave1, §2.3): the `BUZZ_DAEMON_SOCKET` branch is the startup sequence
 * `resolve identity → resolve socket path → connect → GET /health → api_version
 * floor check → attach`, falling back to the spawn path (flock, **post-lock
 * re-connect**, socket-not-pid liveness probe, embedded-daemon materialization,
 * passphrase on the child's stdin, 25 ms poll to 5 s) on ENOENT/ECONNREFUSED.
 * The post-lock re-`connect()` is the whole point of the lock: without it the
 * loser of the race destroys a live socket and produces two daemons on one
 * (relay, identity) — two NIP-42 sessions and two read-state publishers racing
 * on the same `d` coordinate.
 *
 * Until that lands, `BUZZ_TUI_FIXTURE` is the only transport, and it is a
 * product path rather than a test hook: §5.5's tmux sequences and §5.6's
 * recordings drive the shipped binary against a scenario file.
 */

import { render } from "@opentui/solid";
import { FixtureClient } from "./client/fixture-client";
import { Shell } from "./shell/Shell";

/**
 * The clock — §5.3 determinism requirement 1.
 *
 * `BUZZ_TUI_FIXED_TIME` freezes it. Every timestamp the UI renders flows
 * through the function this returns, so a snapshot suite and a tmux capture
 * produce the same bytes on any machine in any timezone.
 */
function resolveClock(): () => number {
  const fixed = process.env.BUZZ_TUI_FIXED_TIME;
  if (!fixed) return () => Date.now();
  const parsed = Number.isNaN(Number(fixed))
    ? Date.parse(fixed)
    : Number(fixed);
  if (Number.isNaN(parsed)) {
    // A malformed value must not silently fall back to the wall clock: that
    // produces a suite that passes locally and diffs in CI for reasons nobody
    // can see. Fail where the mistake was made.
    throw new Error(`BUZZ_TUI_FIXED_TIME is not a time: ${fixed}`);
  }
  return () => parsed;
}

async function main(): Promise<void> {
  const fixture = process.env.BUZZ_TUI_FIXTURE;
  if (!fixture) {
    console.error(
      "buzz-tui: no transport. Set BUZZ_TUI_FIXTURE=<scenario.jsonl> " +
        "(BUZZ_DAEMON_SOCKET lands with the daemon attach path, §2.3).",
    );
    process.exit(1);
  }

  const client = new FixtureClient(await Bun.file(fixture).text());
  const clock = resolveClock();

  await render(
    () => Shell({ client, now: clock, onQuit: () => process.exit(0) }),
    {
      // **`exitOnCtrlC` must be off.** OpenTUI's default is to exit the process
      // on `ctrl+c` before any key handler runs, which silently defeats §5.5's
      // requirement: "`ctrl+c` mid-compose clears the composer and does **not**
      // exit on first press." A chat client that discards a half-written
      // message on the key every terminal user presses reflexively is exactly
      // the small betrayal §5.4's draft persistence exists to prevent — and it
      // would have shipped looking like an OpenTUI behaviour rather than a bug.
      //
      // `Shell` owns the binding instead, and implements both halves.
      exitOnCtrlC: false,
    },
  );
}

await main();
