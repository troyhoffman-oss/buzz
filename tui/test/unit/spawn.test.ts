/**
 * T0 — the §2.3 startup sequence.
 *
 * The socket probe and the lock are driven against a **real** socket and a real
 * filesystem, because both are statements about the filesystem: "connect,
 * do not check a pid" and "exactly one of two racing TUIs spawns" are not
 * observable against a mock.
 *
 * The spawn itself is exercised in `test/tmux/attach.test.ts` against the real
 * `buzz-daemon` binary, since what makes a spawn interesting — stdin delivery,
 * a child that exits, a socket that appears seconds later — needs a real
 * process to be worth asserting.
 */

import { afterEach, describe, expect, test } from "bun:test";
import {
  existsSync,
  mkdtempSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  SPAWN_DEADLINE_MS,
  SPAWN_POLL_INTERVAL_MS,
  SpawnLock,
  findDaemonBinary,
  socketIsLive,
} from "../../src/startup/spawn";
import { configPath, runtimeDir, stateDir } from "../../src/startup/paths";

const cleanup: Array<() => void> = [];

afterEach(() => {
  for (const fn of cleanup.splice(0)) fn();
});

function scratch(): string {
  const dir = mkdtempSync(join(tmpdir(), "buzz-spawn-"));
  cleanup.push(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

describe("liveness is probed on the socket, never on a pid (§2.3)", () => {
  test("a served socket is live", async () => {
    const dir = scratch();
    const socket = join(dir, "daemon.sock");
    const server = Bun.serve({
      unix: socket,
      fetch: () => Response.json({ status: "ok" }),
    });
    cleanup.push(() => server.stop(true));

    expect(await socketIsLive(socket)).toBe(true);
  });

  test("a socket answering 500 is still live", async () => {
    // "connected-but-erroring = alive". Unlinking a daemon's socket because it
    // is unhealthy would replace one broken daemon with two.
    const dir = scratch();
    const socket = join(dir, "daemon.sock");
    const server = Bun.serve({
      unix: socket,
      fetch: () => new Response("boom", { status: 500 }),
    });
    cleanup.push(() => server.stop(true));

    expect(await socketIsLive(socket)).toBe(true);
  });

  test("a missing socket is dead", async () => {
    expect(await socketIsLive(join(scratch(), "never-existed.sock"))).toBe(
      false,
    );
  });

  test("a leftover socket file with nothing listening is dead", async () => {
    // The stale-socket case §2.3 unlinks — and the whole reason it specifies a
    // *connect* probe rather than an errno test is that Bun reports this and
    // ENOENT identically (`code: "FailedToOpenSocket"` for both), so nothing
    // above the probe can tell them apart.
    //
    // `stop()` leaves the socket inode behind on Linux, which is exactly the
    // state a crashed daemon leaves: the path exists, `existsSync` is true, and
    // nobody is listening. A probe that trusted `existsSync` would call this
    // live and the spawn would never happen.
    const dir = scratch();
    const socket = join(dir, "daemon.sock");
    const server = Bun.serve({ unix: socket, fetch: () => new Response("ok") });
    expect(await socketIsLive(socket)).toBe(true);
    server.stop(true);

    expect(existsSync(socket)).toBe(true);
    expect(await socketIsLive(socket)).toBe(false);
  });
});

describe("the spawn lock (§2.3)", () => {
  test("exactly one of two racers takes it", () => {
    // §5.5's own case in its cheapest form: "two TUIs launched within 50 ms
    // yield exactly one daemon pid". The lock is the mechanism.
    const path = join(scratch(), "x.lock");
    const first = SpawnLock.acquire(path, Date.now());
    const second = SpawnLock.acquire(path, Date.now());
    expect(first).not.toBeNull();
    expect(second).toBeNull();
  });

  test("releasing lets the next contender in", () => {
    const path = join(scratch(), "x.lock");
    const first = SpawnLock.acquire(path, Date.now());
    first?.release();
    expect(SpawnLock.acquire(path, Date.now())).not.toBeNull();
  });

  test("release is idempotent, so a finally cannot double-unlink", () => {
    const path = join(scratch(), "x.lock");
    const lock = SpawnLock.acquire(path, Date.now());
    lock?.release();
    lock?.release();
    // The second release must not have removed a *new* holder's lock.
    const next = SpawnLock.acquire(path, Date.now());
    expect(next).not.toBeNull();
    next?.release();
    expect(SpawnLock.acquire(path, Date.now())).not.toBeNull();
  });

  test("a lock older than any live spawn is reclaimed", () => {
    // A crash mid-spawn leaves the file behind. A lock nothing can clear turns
    // one crash into a permanently unlaunchable app — worse than the race it
    // was protecting against.
    const path = join(scratch(), "x.lock");
    const held = SpawnLock.acquire(path, Date.now());
    expect(held).not.toBeNull();

    const ancient = new Date(Date.now() - SPAWN_DEADLINE_MS * 4);
    utimesSync(path, ancient, ancient);
    expect(SpawnLock.acquire(path, Date.now())).not.toBeNull();
  });

  test("a lock inside the deadline is NOT reclaimed", () => {
    // The other half, and the one that matters: reclaiming a live holder's lock
    // is precisely the double-daemon outcome the lock exists to prevent.
    const path = join(scratch(), "x.lock");
    expect(SpawnLock.acquire(path, Date.now())).not.toBeNull();
    const recent = new Date(Date.now() - SPAWN_DEADLINE_MS / 2);
    utimesSync(path, recent, recent);
    expect(SpawnLock.acquire(path, Date.now())).toBeNull();
  });
});

describe("the spawn deadline is measured, not guessed", () => {
  test("the poll interval is §2.3's 25 ms", () => {
    expect(SPAWN_POLL_INTERVAL_MS).toBe(25);
  });

  test("the deadline exceeds a measured cold start with real headroom", () => {
    // §2.3 writes 5 s. A NIP-49 decrypt at log-n 18 was measured at 1.38 s from
    // exec to a bound socket in a release build on this machine — 3.6x headroom
    // on an *idle* box, and the flagship install is a VPS running agent fleets
    // this fork has already measured at ~2.8 GB across 48 bridges.
    //
    // The number is asserted rather than left to a comment because the
    // temptation to "restore the spec value" is real, and the failure it
    // reintroduces is nasty: the TUI reports a spawn failure while the daemon
    // keeps starting behind it, so the *next* launch works and the bug reads
    // as intermittent.
    const MEASURED_COLD_START_MS = 1380;
    expect(SPAWN_DEADLINE_MS).toBeGreaterThanOrEqual(
      MEASURED_COLD_START_MS * 10,
    );
  });
});

describe("binary discovery names all three places (§2.3, §6.5)", () => {
  test("an explicit BUZZ_DAEMON_BIN wins", () => {
    const dir = scratch();
    const binary = join(dir, "buzz-daemon");
    writeFileSync(binary, "#!/bin/sh\n");
    expect(findDaemonBinary({ BUZZ_DAEMON_BIN: binary })).toBe(binary);
  });

  test("a BUZZ_DAEMON_BIN pointing at nothing falls through rather than failing", () => {
    // Falling through is right: a stale env var in a shell profile must not
    // make the app unlaunchable when a perfectly good binary is on PATH.
    const missing = join(scratch(), "not-here");
    const found = findDaemonBinary({ BUZZ_DAEMON_BIN: missing });
    expect(found).not.toBe(missing);
  });
});

describe("paths (§2.2)", () => {
  test("XDG_STATE_HOME is honoured for the TUI's own state", () => {
    expect(stateDir({ XDG_STATE_HOME: "/x/state" })).toBe("/x/state/buzz");
    expect(configPath({ XDG_STATE_HOME: "/x/state" })).toBe(
      "/x/state/buzz/config.json",
    );
  });

  test("the runtime directory follows XDG_RUNTIME_DIR on Linux", () => {
    expect(runtimeDir({ XDG_RUNTIME_DIR: "/run/user/1000" }, "linux")).toBe(
      "/run/user/1000/buzz",
    );
  });

  test("no XDG_RUNTIME_DIR falls back into the state directory", () => {
    // Normal under `su`, in a container, and in CI. Refusing to launch there
    // would make the daemon unusable in exactly the environments §6.5 targets.
    expect(runtimeDir({ XDG_STATE_HOME: "/x/state" }, "linux")).toBe(
      "/x/state/buzz/run",
    );
  });

  test("macOS uses Application Support rather than XDG", () => {
    const path = runtimeDir({ XDG_RUNTIME_DIR: "/run/user/1000" }, "darwin");
    expect(path).toContain("Library/Application Support/buzz/run");
    expect(path).not.toContain("/run/user");
  });
});
