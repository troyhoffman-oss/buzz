/**
 * `buzz-tui` entry point — DESIGN.md §2.3, §2.5, §5.3, §5.4, and §4's
 * first-run-onboarding directive.
 *
 * **The user runs `buzz-tui`.** There is no "start the daemon first", no port
 * to pick, and no config file to author before first use. On a machine that has
 * never seen Buzz, plain `buzz-tui` opens a welcome flow; on one that has, it
 * attaches to a running daemon in about a millisecond, or spawns one.
 *
 * # Four ways in, in the order they are checked
 *
 * | Condition | Path |
 * |---|---|
 * | `BUZZ_TUI_FIXTURE` | the fixture transport (§5.4) — deterministic, T1/T2 |
 * | `BUZZ_DAEMON_SOCKET` | attach to that socket verbatim (§6.5's `ssh -L`) |
 * | a config naming a provisioned identity | resolve → attach → else spawn (§2.3) |
 * | nothing | the onboarding wizard, then the row above |
 *
 * `BUZZ_TUI_FIXTURE` is checked **first and unconditionally**, which is what
 * keeps §5.5's tmux sequences deterministic: a fixture run on a developer's box
 * must not notice their real config and quietly attach to their real daemon.
 *
 * # Exiting 1 is now reserved for actual failures
 *
 * The previous revision exited 1 whenever `BUZZ_TUI_FIXTURE` was unset, which
 * made a first launch on a clean machine indistinguishable from a crash. §1.3
 * property 2 is explicit that a dead end is a defect, and "no transport" was
 * one: the remedy existed (onboard) and the program did not offer it.
 */

import { render } from "@opentui/solid";
import type { DaemonClient } from "./client/daemon-client";
import { FixtureClient } from "./client/fixture-client";
import { UdsClient } from "./client/uds-client";
import { Wizard } from "./onboarding/Wizard";
import {
  listIdentities,
  resolveSocketPaths,
  writeConfig,
} from "./onboarding/provision";
import { Shell } from "./shell/Shell";
import { type TuiConfig, configPath } from "./startup/paths";
import { attachOrSpawn, findDaemonBinary, socketIsLive } from "./startup/spawn";

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

/** Read the TUI's config, or `null` when this machine has never onboarded. */
async function readConfig(): Promise<TuiConfig | null> {
  const file = Bun.file(configPath());
  if (!(await file.exists())) return null;
  try {
    const parsed = (await file.json()) as Partial<TuiConfig>;
    if (!parsed.relayUrl || !parsed.pubkey) return null;
    return {
      relayUrl: parsed.relayUrl,
      pubkey: parsed.pubkey,
      communityName: parsed.communityName ?? "buzz",
    };
  } catch {
    // A corrupt config is treated as absent so the wizard can rewrite it.
    // Refusing to start would leave an operator holding a file they have no
    // tool to repair, which is the dead end §1.3 property 2 forbids.
    return null;
  }
}

/**
 * Ask for the identity passphrase on the controlling terminal.
 *
 * §2.5's rule as code: the passphrase is **prompted for**, never read from
 * argv or the environment, and it goes straight into the spawn's stdin.
 *
 * This deliberately runs **before** OpenTUI takes the screen. OpenTUI puts the
 * terminal in raw mode and paints frames, so a passphrase field rendered inside
 * it would need echo suppression fought for inside somebody else's render loop;
 * here it is a plain read against the TTY with echo simply never happening.
 *
 * §2.5's "N communities, one passphrase prompt" is why this returns the value
 * rather than writing it anywhere: one prompt feeds every spawn in a fan-out.
 */
async function promptPassphrase(pubkeyPrefix: string): Promise<string> {
  process.stdout.write(`passphrase for identity ${pubkeyPrefix}: `);
  const isTty = process.stdin.isTTY === true;
  if (isTty) process.stdin.setRawMode(true);
  let line = "";
  for await (const chunk of process.stdin) {
    for (const byte of chunk as Uint8Array) {
      // Enter ends the line; ctrl+c aborts; backspace edits. **Nothing is
      // echoed** — a passphrase in a scrollback buffer is a passphrase on disk,
      // and `capture-pane` is a grep over exactly that buffer.
      if (byte === 0x0d || byte === 0x0a) {
        if (isTty) process.stdin.setRawMode(false);
        process.stdout.write("\n");
        return line;
      }
      if (byte === 0x03) {
        if (isTty) process.stdin.setRawMode(false);
        process.stdout.write("\n");
        process.exit(130);
      }
      if (byte === 0x7f || byte === 0x08) {
        line = line.slice(0, -1);
        continue;
      }
      if (byte >= 0x20) line += String.fromCharCode(byte);
    }
  }
  if (isTty) process.stdin.setRawMode(false);
  return line;
}

/** Boot the app against a connected client. */
async function runShell(client: DaemonClient): Promise<void> {
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

/** Run the first-run wizard, returning the config it wrote. */
async function runOnboarding(daemonBinary: string): Promise<TuiConfig> {
  return new Promise<TuiConfig>((resolve) => {
    void render(
      () =>
        Wizard({
          daemonBinary,
          onQuit: () => process.exit(0),
          onComplete: (result) => {
            const config: TuiConfig = {
              relayUrl: result.relayUrl,
              pubkey: result.pubkey,
              communityName: result.communityName,
            };
            writeConfig(configPath(), config);
            resolve(config);
          },
        }),
      { exitOnCtrlC: false },
    );
  });
}

/**
 * The §2.3 sequence: resolve → connect → health → attach, else spawn.
 *
 * **The liveness probe runs before the prompt**, and that ordering is the whole
 * of §2.3's "fast path, ~1 ms". A daemon already running needs no passphrase —
 * it decrypted the key when it started — so asking first would charge every
 * launch of an always-on VPS install (§6.5) for a cold start it never performs.
 * Worse, the empty passphrase would then be *spent*: the daemon runs the full
 * ~1.4 s scrypt before discovering it is wrong.
 */
async function attach(
  config: TuiConfig,
  daemonBinary: string,
): Promise<UdsClient> {
  const paths = await resolveSocketPaths(
    daemonBinary,
    config.relayUrl,
    config.pubkey,
  );

  if (!(await socketIsLive(paths.socket))) {
    const passphrase = await promptPassphrase(config.pubkey.slice(0, 8));
    const outcome = await attachOrSpawn({
      binary: daemonBinary,
      socket: paths.socket,
      lock: paths.lock,
      relayUrl: config.relayUrl,
      pubkey: config.pubkey,
      passphrase,
      ...(idleTimeoutFromEnv() !== undefined
        ? { idleTimeoutSecs: idleTimeoutFromEnv() as number }
        : {}),
    });
    if (outcome.kind === "failed") {
      // §2.3: "Spawn failure is a first-class outcome, not a timeout." The
      // reason is the child's own last stderr line — a wrong passphrase says
      // so, rather than reporting a five-second wait.
      console.error(`buzz-tui: ${outcome.reason}`);
      process.exit(1);
    }
  }

  return UdsClient.connect({
    socket: paths.socket,
    relayUrl: config.relayUrl,
    communityName: config.communityName,
    now: resolveClock(),
  });
}

/** `BUZZ_TUI_IDLE_TIMEOUT` in seconds; `0` disables it (§2.3, §6.5). */
function idleTimeoutFromEnv(): number | undefined {
  const raw = process.env.BUZZ_TUI_IDLE_TIMEOUT;
  if (raw === undefined) return undefined;
  const parsed = Number(raw);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : undefined;
}

/**
 * Resolve the config, onboarding when there is none.
 *
 * The one subtlety: an identity can exist without a config — provisioned by
 * another front end, or by a run that crashed after writing the blob.
 * Onboarding in that case would mint a **second** identity beside the first,
 * so an existing identity plus a `BUZZ_RELAY_URL` adopts rather than re-asks.
 */
async function resolveConfig(daemonBinary: string): Promise<TuiConfig> {
  const existing = await readConfig();
  if (existing) return existing;

  const identities = await listIdentities(daemonBinary);
  const pubkey = identities[0];
  const relayUrl = process.env.BUZZ_RELAY_URL;
  if (!pubkey || !relayUrl) return runOnboarding(daemonBinary);

  const adopted: TuiConfig = {
    relayUrl,
    pubkey,
    communityName: process.env.BUZZ_COMMUNITY_NAME ?? "buzz",
  };
  writeConfig(configPath(), adopted);
  return adopted;
}

async function main(): Promise<void> {
  // §5.4's fixture transport, checked first so a T1/T2 run cannot be perturbed
  // by whatever config or daemon happens to exist on the machine.
  const fixture = process.env.BUZZ_TUI_FIXTURE;
  if (fixture) {
    await runShell(new FixtureClient(await Bun.file(fixture).text()));
    return;
  }

  // §6.5's `ssh -L` shape: an explicit socket is taken verbatim, with no spawn
  // and no config, because the daemon is on another machine and nothing here
  // could start it.
  const explicitSocket = process.env.BUZZ_DAEMON_SOCKET;
  if (explicitSocket) {
    await runShell(
      await UdsClient.connect({
        socket: explicitSocket,
        now: resolveClock(),
      }),
    );
    return;
  }

  const daemonBinary = findDaemonBinary();
  if (!daemonBinary) {
    // All three places it was looked for, because "not found" without them is
    // a dead end and the fix is usually one of the three.
    console.error(
      "buzz-tui: cannot find buzz-daemon. Install it beside buzz-tui, " +
        "put it on PATH, or set BUZZ_DAEMON_BIN=<path>.",
    );
    process.exit(1);
  }

  const config = await resolveConfig(daemonBinary);
  await runShell(await attach(config, daemonBinary));
}

await main();
