/**
 * `buzz-tui` entry point — DESIGN.md §2.3, §3.0.
 *
 * **The user runs `buzz-tui`.** There is no "start the daemon first", no port
 * to pick, no config file to author before first use. The daemon is an
 * implementation detail that happens to survive Ctrl-C (§1.3 property 1).
 *
 * TODO(wave1, §2.3): the startup sequence is
 * `resolve identity → resolve socket path → connect → GET /health → api_version
 * floor check → attach`, falling back to the spawn path (flock, **post-lock
 * re-connect**, socket-not-pid liveness probe, embedded-daemon materialization,
 * passphrase on the child's stdin, 25 ms poll to 5 s) on ENOENT/ECONNREFUSED.
 * The post-lock re-`connect()` is the whole point of the lock: without it the
 * loser of the race destroys a live socket and produces two daemons on one
 * (relay, identity) — two NIP-42 sessions and two read-state publishers racing
 * on the same 30078 `d` coordinate.
 *
 * This scaffold boots the §3.0 shell against no daemon so the frame, the tier
 * table, and the quit path are exercisable before any of that lands.
 */

import { render } from "@opentui/solid";
import { Shell } from "./shell/Shell";

async function main(): Promise<void> {
  await render(() => Shell({ onQuit: () => process.exit(0) }));
}

await main();
