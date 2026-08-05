/**
 * The §2.3 startup sequence: attach, or win a lock and spawn.
 *
 * ```
 * buzz-tui start
 *  ├─ resolve identity → resolve socket path
 *  ├─ connect(socket)
 *  │   ├─ ok → GET /health → api_version floor check → attach  (fast path)
 *  │   └─ ENOENT | ECONNREFUSED
 *  │        ├─ flock(<hash>.lock)                # two TUIs racing → one spawns
 *  │        ├─ connect(socket) AGAIN             # ← load-bearing
 *  │        ├─ liveness-probe the socket, not the pid
 *  │        ├─ unlink socket only on confirmed-dead
 *  │        ├─ spawn buzz-daemon --socket <path> --detach
 *  │        ├─ write the PASSPHRASE to the child's stdin, close it
 *  │        ├─ poll connect(), terminating early on child exit
 *  │        └─ unlock
 *  └─ GET /session → first paint
 * ```
 *
 * # The post-lock re-connect is the whole point of the lock
 *
 * §2.3 says it plainly, and it is the one step an implementation is most likely
 * to drop as redundant:
 *
 * > Without it the loser of the race acquires the lock *after* the winner has
 * > released it and proceeds straight to unlink-and-spawn — destroying a live
 * > socket and producing two daemons on one (relay, identity). That is not a
 * > cosmetic duplicate: two NIP-42 sessions, two relay-budget consumers, and
 * > two read-state publishers racing on the same 30078 `d` coordinate.
 *
 * # Liveness is probed on the socket, never on a pid
 *
 * A pidfile pid check is a PID-reuse race. `connect()` refused or ENOENT means
 * dead; connected-but-erroring means alive. Both are observable here through
 * one `fetch`, which is why this module never reads the pidfile at all.
 */

import { spawnSync } from "node:child_process";
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  statSync,
  unlinkSync,
} from "node:fs";
import { MS_PER_SECOND } from "../time/units";

/** How often the spawn poll re-tries `connect()` (§2.3: "@25 ms"). */
export const SPAWN_POLL_INTERVAL_MS = 25;

/**
 * How long the spawn poll waits before giving up.
 *
 * **§2.3 says 5 s; this is 20 s, and the deviation is measured rather than
 * cautious.** The daemon's first act is a NIP-49 scrypt decrypt at log-n 18
 * (~256 MiB), which was timed on this box at **1.38 s from exec to a bound
 * socket** in a release build. Five seconds is 3.6× headroom on an idle
 * machine — and the flagship install (§6.5) is a VPS running agent fleets, a
 * population this fork has already measured at ~2.8 GB across 48 bridges.
 * Under that pressure a 256 MiB KDF can exceed 5 s, and the resulting failure
 * is worse than slow: the TUI reports a spawn failure while the daemon keeps
 * starting behind it, so the *next* launch attaches fine and the bug reads as
 * intermittent.
 *
 * What makes the longer ceiling safe is that it is not the primary exit.
 * {@link waitForSocket} terminates the moment the child exits, which is §2.3's
 * own rule — so a wrong passphrase or a corrupt blob still fails in
 * milliseconds with the child's real message. This deadline only ever elapses
 * for a child that is alive and working.
 */
export const SPAWN_DEADLINE_MS = 20 * MS_PER_SECOND;

/** How a startup attempt ended. */
export type AttachOutcome =
  | { readonly kind: "attached"; readonly socket: string }
  | { readonly kind: "spawned"; readonly socket: string }
  /** Nothing is listening and this process did not win the lock. */
  | { readonly kind: "failed"; readonly reason: string };

/**
 * Probe a socket by connecting to it (§2.3's liveness rule).
 *
 * Returns `true` for **connected-but-erroring** as well as for a clean
 * response: a daemon answering 500 is alive, and unlinking its socket because
 * it is unhealthy would replace one broken daemon with two.
 */
export async function socketIsLive(socket: string): Promise<boolean> {
  if (!existsSync(socket)) return false;
  try {
    await fetch("http://buzz-daemon/health", {
      unix: socket,
      signal: AbortSignal.timeout(2 * MS_PER_SECOND),
    });
    return true;
  } catch {
    // Bun reports both ENOENT and connection-refused as `FailedToOpenSocket`,
    // so the two cannot be told apart by code — which is exactly why §2.3
    // specifies a *connect* probe rather than an errno test. Either way, dead.
    return false;
  }
}

/**
 * An advisory lock held for the duration of a spawn (§2.3).
 *
 * `O_CREAT | O_EXCL` rather than `flock(2)`: the property §2.3 needs is
 * "exactly one of two racing TUIs spawns", and an exclusive create gives that
 * with no FFI, no libc name to resolve per platform, and no risk of the lock
 * evaporating when a file descriptor is inherited by the daemon we are about to
 * spawn — which is precisely what `flock` does, and would silently unlock the
 * race for the *next* contender while the first is still polling.
 *
 * The cost of `O_EXCL` is that a crash leaves the file behind. That is handled
 * by {@link SpawnLock.acquire}'s staleness check rather than ignored, because a
 * lock nothing can clear turns one crash into a permanently unlaunchable app.
 */
export class SpawnLock {
  private readonly path: string;
  private held = false;

  private constructor(path: string) {
    this.path = path;
  }

  /**
   * Try to take the lock. Returns `null` when another process holds it.
   *
   * A lock file older than {@link SPAWN_DEADLINE_MS} is treated as abandoned
   * and removed: no legitimate holder outlives its own spawn deadline, so the
   * only thing an older file can be is the remains of a crash.
   */
  static acquire(path: string, nowMs: number): SpawnLock | null {
    mkdirSync(dirOf(path), { recursive: true, mode: 0o700 });
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        const fd = openSync(path, "wx", 0o600);
        closeSync(fd);
        const lock = new SpawnLock(path);
        lock.held = true;
        return lock;
      } catch {
        if (attempt > 0) return null;
        // Clear a lock that no live spawn could still be holding, then retry
        // exactly once. Retrying more would be a spin against a live holder.
        if (!isStale(path, nowMs)) return null;
        try {
          unlinkSync(path);
        } catch {
          // Another process cleared it first, which is the same outcome.
        }
      }
    }
    return null;
  }

  /** Release the lock. Idempotent, so a `finally` cannot double-unlink. */
  release(): void {
    if (!this.held) return;
    this.held = false;
    try {
      unlinkSync(this.path);
    } catch {
      // Already gone. The lock's job is done either way, and throwing here
      // would turn a successful spawn into a startup failure.
    }
  }
}

function dirOf(path: string): string {
  const cut = path.lastIndexOf("/");
  return cut > 0 ? path.slice(0, cut) : "/";
}

function isStale(path: string, nowMs: number): boolean {
  try {
    return nowMs - statSync(path).mtimeMs > SPAWN_DEADLINE_MS;
  } catch {
    // The file vanished between the failed create and this stat, which means
    // the holder released it. Reporting "not stale" sends the caller round the
    // retry once, which is the right outcome.
    return false;
  }
}

/** What a spawn needs to know. */
export interface SpawnRequest {
  /** Path of the `buzz-daemon` binary. */
  readonly binary: string;
  /** Socket the daemon will bind. */
  readonly socket: string;
  /** Lock path for this (relay, identity) pair. */
  readonly lock: string;
  /** Relay websocket URL — argv, because it is not a secret. */
  readonly relayUrl: string;
  /** Pubkey of the provisioned identity — argv, also not a secret. */
  readonly pubkey: string;
  /**
   * Passphrase for the identity blob.
   *
   * Goes to the child's **stdin**, and this is the only place it is held. §2.5:
   * "nothing secret is ever an argument" — not in argv (world-readable in
   * `/proc`), not in an inherited environment.
   */
  readonly passphrase: string;
  /** `--idle-timeout` in seconds; `0` disables it (§2.3, §6.5). */
  readonly idleTimeoutSecs?: number;
}

/** A spawned child, as far as the poll loop needs to see it. */
interface SpawnedChild {
  /** Resolves when the process exits. */
  readonly exited: Promise<number>;
  /** Everything the child wrote to stderr, for the failure message. */
  stderr(): Promise<string>;
  kill(): void;
}

/**
 * Start a daemon and wait for its socket.
 *
 * The whole §2.3 fallback, in order, with the post-lock re-connect intact.
 */
export async function attachOrSpawn(
  request: SpawnRequest,
  nowMs: () => number = Date.now,
): Promise<AttachOutcome> {
  if (await socketIsLive(request.socket)) {
    return { kind: "attached", socket: request.socket };
  }

  const lock = SpawnLock.acquire(request.lock, nowMs());
  if (!lock) {
    // Someone else is spawning. Wait for *their* daemon rather than queueing
    // for the lock: the outcome we want is a live socket, and whoever produces
    // it is irrelevant. Queueing would serialize two spawns of the same daemon.
    const appeared = await waitForSocket(request.socket, null, nowMs);
    return appeared
      ? { kind: "attached", socket: request.socket }
      : {
          kind: "failed",
          reason:
            "another buzz-tui is starting this daemon and it has not come up; " +
            `check ${request.lock}`,
        };
  }

  try {
    // **The load-bearing re-connect.** The winner may have spawned and released
    // while this process was blocked on the lock. Without this line the loser
    // proceeds to unlink a live socket and start a second daemon on one
    // (relay, identity).
    if (await socketIsLive(request.socket)) {
      return { kind: "attached", socket: request.socket };
    }

    // Confirmed dead — `socketIsLive` just probed it — so a leftover socket
    // file is safe to remove. Unlinking without that probe is what turns a
    // stale-socket cleanup into a live-daemon outage.
    if (existsSync(request.socket)) {
      try {
        unlinkSync(request.socket);
      } catch {
        // A concurrent cleanup got there first; bind will tell us if not.
      }
    }

    const child = startDaemon(request);
    const appeared = await waitForSocket(request.socket, child, nowMs);
    if (appeared) return { kind: "spawned", socket: request.socket };

    // §2.3: "Spawn failure is a first-class outcome, not a timeout." The
    // child's own stderr is the message, because "timed out after 5 s" for a
    // wrong passphrase is the dead end §1.3 property 2 forbids.
    child.kill();
    const stderr = (await child.stderr()).trim();
    return {
      kind: "failed",
      reason:
        stderr.length > 0
          ? `buzz-daemon exited: ${lastLine(stderr)}`
          : `buzz-daemon did not bind ${request.socket}`,
    };
  } finally {
    lock.release();
  }
}

/** The last non-empty line of a child's stderr — the actual error, not the log. */
function lastLine(text: string): string {
  const lines = text.split("\n").filter((l) => l.trim().length > 0);
  return lines.at(-1) ?? text;
}

/**
 * Launch `buzz-daemon` and feed it the passphrase on stdin.
 *
 * `stderr: "pipe"` because the failure path needs it; `stdout: "ignore"`
 * because this process owns the terminal and a daemon log line written into it
 * would corrupt the frame.
 */
function startDaemon(request: SpawnRequest): SpawnedChild {
  const args = [
    "--socket",
    request.socket,
    "--relay",
    request.relayUrl,
    "--identity",
    request.pubkey,
    "--passphrase-stdin",
    "--detach",
  ];
  if (request.idleTimeoutSecs !== undefined) {
    args.push("--idle-timeout", String(request.idleTimeoutSecs));
  }

  const child = Bun.spawn([request.binary, ...args], {
    stdin: "pipe",
    stdout: "ignore",
    stderr: "pipe",
  });

  // One line, then close — §2.5 path 1 verbatim. Closing matters: the daemon
  // reads exactly one line and a stdin left open would leave it holding a pipe
  // to a process that has moved on.
  child.stdin.write(`${request.passphrase}\n`);
  void child.stdin.end();

  let stderrText: Promise<string> | null = null;
  return {
    exited: child.exited,
    stderr: () => {
      // Memoized: the body is a stream, and reading it twice returns empty the
      // second time — which would silently blank the one message that explains
      // a spawn failure.
      stderrText ??= new Response(child.stderr).text();
      return stderrText;
    },
    kill: () => child.kill(),
  };
}

/**
 * Poll `connect()` until the socket answers, the child exits, or time runs out.
 *
 * The child-exit branch is what makes {@link SPAWN_DEADLINE_MS} safe to set
 * generously: every *fast* failure (wrong passphrase, corrupt blob, unsafe
 * socket directory) ends the wait immediately with the child's own reason.
 */
export async function waitForSocket(
  socket: string,
  child: SpawnedChild | null,
  nowMs: () => number = Date.now,
): Promise<boolean> {
  const deadline = nowMs() + SPAWN_DEADLINE_MS;
  let childExited = false;
  if (child) void child.exited.then(() => (childExited = true));

  while (nowMs() < deadline) {
    if (await socketIsLive(socket)) return true;
    if (childExited) {
      // One last probe after the exit: with `--detach` the process that exits
      // may be a parent whose child bound the socket, so declaring failure on
      // the exit alone would report a spawn that actually worked.
      return socketIsLive(socket);
    }
    await Bun.sleep(SPAWN_POLL_INTERVAL_MS);
  }
  return false;
}

/**
 * Locate the `buzz-daemon` binary (§2.3's "materialize embedded daemon binary
 * if absent", in the shape this build has).
 *
 * Order: an explicit `BUZZ_DAEMON_BIN`, then the directory holding this
 * executable (which is how the released pair ships — two binaries side by side
 * in `~/.local/bin`, §6.5), then `PATH`. Returns `null` rather than throwing so
 * the caller can produce a message naming all three, which is the difference
 * between "not found" and a dead end.
 */
export function findDaemonBinary(
  env: NodeJS.ProcessEnv = process.env,
): string | null {
  const explicit = env.BUZZ_DAEMON_BIN;
  if (explicit && existsSync(explicit)) return explicit;

  const sibling = `${dirOf(process.execPath)}/buzz-daemon`;
  if (existsSync(sibling)) return sibling;

  // `which` rather than walking `PATH` by hand: it already honours `PATHEXT`,
  // symlinks, and the executable bit, and getting any of those subtly wrong
  // produces a "found" binary that fails to exec.
  const found = spawnSync("sh", ["-c", "command -v buzz-daemon"], {
    encoding: "utf8",
  });
  const path = found.stdout?.trim();
  return path && path.length > 0 && existsSync(path) ? path : null;
}
