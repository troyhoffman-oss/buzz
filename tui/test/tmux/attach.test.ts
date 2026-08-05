/**
 * T2 — the daemon transport and first-run onboarding, in a real PTY.
 *
 * These drive the **shipped entry point** against a **real `buzz-daemon`
 * binary** and assert on screens, never on internals. Three sequences the brief
 * names, each unreachable from a unit test:
 *
 * 1. **Attach to a running daemon** — the fast path of §2.3, and the only place
 *    the whole chain (socket-path derivation → `/health` → floor check →
 *    snapshot → paint) runs end to end against real processes.
 * 2. **Spawn when absent** — passphrase on the child's stdin, a poll that
 *    outlasts a ~1.4 s scrypt, and a frame that appears afterwards.
 * 3. **The onboarding walk** — a brand-new machine, no config and no env, must
 *    reach a welcome screen rather than exiting 1.
 *
 * # Why these need a real binary
 *
 * The unit suites use a fake daemon on a real socket, which covers the wire.
 * What they cannot cover is the part that made the previous revision's startup
 * untestable: whether the passphrase actually reaches a child's stdin, whether
 * the `--identity` flag resolves the blob the provisioning step wrote, and
 * whether a scrypt that takes seconds still lands inside the poll. Each of
 * those is a claim about two processes.
 *
 * The binary is built once by the harness. If `cargo` is unavailable the suite
 * **skips loudly** rather than passing — a suite that no-ops and reports green
 * is worse than none, because it looks like coverage.
 */

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CWD = new URL("../../", import.meta.url).pathname;
const REPO = new URL("../../../", import.meta.url).pathname;

/** Where the release daemon lands, honouring the lane's `CARGO_TARGET_DIR`. */
function daemonBinaryPath(): string {
  const target = process.env.CARGO_TARGET_DIR ?? join(REPO, "target");
  return join(target, "release", "buzz-daemon");
}

let daemonBinary = "";

beforeAll(() => {
  const built = daemonBinaryPath();
  if (existsSync(built)) {
    daemonBinary = built;
    return;
  }
  // A **release** build specifically: scrypt at log-n 18 takes ~1.4 s
  // optimized and over 100 s in a debug build, which no poll deadline should
  // be sized for and which would make this suite unrunnable.
  const build = Bun.spawnSync(
    ["cargo", "build", "--release", "-p", "buzz-daemon"],
    { cwd: REPO, env: process.env },
  );
  if (build.exitCode === 0 && existsSync(built)) daemonBinary = built;
});

/** A driven tmux session, mirroring `test/tmux/drive.test.ts`'s harness. */
class Pane {
  private readonly session: string;
  readonly home: string;

  constructor(
    command: string,
    env: Record<string, string>,
    cols = 100,
    rows = 30,
    /**
     * Working directory for the pane.
     *
     * Defaults to `tui/`, which is right for `bun run src/main.ts`. A
     * **compiled** binary must be given somewhere else: it re-reads whatever
     * `bunfig.toml` is in its CWD and cannot resolve the dev-only preload from
     * its embedded graph, so running it from `tui/` fails with
     * `preload not found` even though the transform is baked in.
     */
    cwd: string = CWD,
  ) {
    this.session = `buzz-tui-attach-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
    this.home = env.HOME ?? CWD;

    const exported = Object.entries(env)
      .map(([key, value]) => `${key}=${shellQuote(value)}`)
      .join(" ");

    this.tmux([
      "new-session",
      "-d",
      "-s",
      this.session,
      "-c",
      cwd,
      "-x",
      String(cols),
      "-y",
      String(rows),
      // **The trailing `sleep` is load-bearing, and the obvious alternative
      // does not work.** A test whose subject is a *failure message* must be
      // able to capture the pane after the process exits. `remain-on-exit on`
      // looks like the answer and is not: tmux **clears the pane** on death
      // and replaces it with `Pane is dead (status 1, …)`, so the message the
      // test exists to read is gone — and it looks like the app printed
      // nothing, which is exactly the bug being tested for.
      //
      // Keeping the *shell* alive keeps the already-written stderr on screen,
      // because nothing has repainted over it. The session is killed in
      // `afterEach` either way, so the sleep never outlives the test.
      `${exported} ${command}; sleep 300`,
    ]);

    // `-x/-y` at creation is not enough on a box already running tmux.
    this.tmux(["set-option", "-t", this.session, "window-size", "manual"]);
    this.tmux([
      "resize-window",
      "-t",
      this.session,
      "-x",
      String(cols),
      "-y",
      String(rows),
    ]);
  }

  private tmux(args: string[]): string {
    const result = Bun.spawnSync(["tmux", ...args], { cwd: CWD });
    if (result.exitCode !== 0) {
      throw new Error(
        `tmux ${args[0]} failed: ${new TextDecoder().decode(result.stderr)}`,
      );
    }
    return new TextDecoder().decode(result.stdout);
  }

  capture(): string {
    return this.tmux(["capture-pane", "-p", "-t", this.session]);
  }

  send(keys: string): void {
    this.tmux(["send-keys", "-t", this.session, keys]);
  }

  type(text: string): void {
    this.tmux(["send-keys", "-t", this.session, "-l", text]);
  }

  /** Poll for a marker rather than sleeping (§5.5). */
  async waitFor(marker: string, timeoutMs = 30_000): Promise<string> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const pane = this.capture();
      if (pane.includes(marker)) return pane;
      if (Date.now() > deadline) {
        throw new Error(
          `timed out waiting for ${JSON.stringify(marker)}\n--- pane ---\n${pane}`,
        );
      }
      await Bun.sleep(100);
    }
  }

  kill(): void {
    Bun.spawnSync(["tmux", "kill-session", "-t", this.session]);
  }
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

/** An isolated machine: its own HOME, XDG dirs, and identity directory. */
class Machine {
  readonly root: string;
  readonly env: Record<string, string>;

  constructor() {
    this.root = mkdtempSync(join(tmpdir(), "buzz-machine-"));
    this.env = {
      HOME: this.root,
      XDG_DATA_HOME: join(this.root, "data"),
      XDG_STATE_HOME: join(this.root, "state"),
      XDG_RUNTIME_DIR: join(this.root, "run"),
      BUZZ_DAEMON_BIN: daemonBinary,
      BUZZ_TUI_FIXED_TIME: "2026-08-04T14:12:00Z",
      TZ: "UTC",
      LANG: "C.UTF-8",
      PATH: process.env.PATH ?? "/usr/bin:/bin",
    };
  }

  /** Run a daemon subcommand on this machine, returning parsed stdout. */
  run(args: string[], stdin?: string): Record<string, unknown> {
    const child = Bun.spawnSync([daemonBinary, ...args], {
      env: this.env,
      ...(stdin !== undefined ? { stdin: Buffer.from(`${stdin}\n`) } : {}),
    });
    const stdout = new TextDecoder().decode(child.stdout);
    if (child.exitCode !== 0) {
      throw new Error(
        `buzz-daemon ${args.join(" ")} failed: ${new TextDecoder().decode(child.stderr)}`,
      );
    }
    const line = stdout
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l.startsWith("{"))
      .at(-1);
    return line ? (JSON.parse(line) as Record<string, unknown>) : {};
  }

  destroy(): void {
    rmSync(this.root, { recursive: true, force: true });
  }
}

const PASSPHRASE = "correct horse battery";
const RELAY = "wss://relay.invalid";

let pane: Pane | null = null;
let machine: Machine | null = null;
let daemonProcess: Bun.Subprocess<"pipe", "ignore", "ignore"> | null = null;

afterEach(() => {
  pane?.kill();
  pane = null;
  daemonProcess?.kill();
  daemonProcess = null;
  machine?.destroy();
  machine = null;
});

/** Launch the TUI from source in a pane on `m`. */
function launch(m: Machine, extra: Record<string, string> = {}): Pane {
  pane = new Pane("bun run src/main.ts", { ...m.env, ...extra });
  return pane;
}

/**
 * Launch the **compiled** binary, when one has been built.
 *
 * `scripts/build.ts` writes it; `bun run build x86_64-unknown-linux-gnu` is the
 * command. Returns `null` when it is absent, so the case below skips rather
 * than failing on a developer box that has not compiled.
 *
 * The compiled binary runs from a **scratch directory**, never from `tui/`:
 * `bunfig.toml`'s preload is dev-only and a compiled binary re-reads whatever
 * `bunfig.toml` sits in its CWD, failing with `preload not found` even though
 * the transform is already baked in. `scripts/smoke.sh` carries the same note.
 */
function launchCompiled(m: Machine): Pane | null {
  const binary = join(CWD, "dist", "buzz-tui-x86_64-unknown-linux-gnu");
  if (!existsSync(binary)) return null;
  pane = new Pane(binary, m.env, 100, 30, m.root);
  return pane;
}

describe("§2.3 attach and spawn, against a real daemon", () => {
  test("a brand-new machine reaches a welcome screen and does NOT exit 1", async () => {
    // The regression this whole lane exists for. The previous revision's
    // `main.ts:52` exited 1 whenever `BUZZ_TUI_FIXTURE` was unset, so a first
    // launch on a clean machine was indistinguishable from a crash — and the
    // remedy (onboard) existed but was never offered, which is the dead end
    // §1.3 property 2 forbids.
    if (!daemonBinary) throw new Error("buzz-daemon was not built; cannot run");
    machine = new Machine();
    const p = launch(machine);

    const frame = await p.waitFor("Welcome to Buzz");
    // The promise that matters most on a first screen, asserted as pixels.
    expect(frame).toContain("never sent anywhere");
    // The way forward is on screen, in the hint band [G14].
    expect(frame).toContain("⏎ begin");
  }, 90_000);

  test("the onboarding walk reaches a live shell, with no secret on screen", async () => {
    if (!daemonBinary) throw new Error("buzz-daemon was not built; cannot run");
    machine = new Machine();
    const p = launch(machine);
    await p.waitFor("Welcome to Buzz");

    p.send("Enter");
    await p.waitFor("Which relay?");
    p.type(RELAY);
    await p.waitFor(RELAY);

    p.send("Enter");
    await p.waitFor("Create an identity");
    p.send("Enter"); // create

    await p.waitFor("Choose a passphrase");
    p.type(PASSPHRASE);
    // Masked, and the mask is per-grapheme so a keystroke is visibly landing.
    const masked = await p.waitFor("•".repeat(PASSPHRASE.length));
    expect(masked).not.toContain(PASSPHRASE);

    p.send("Enter");
    await p.waitFor("Type it again");
    p.type(PASSPHRASE);
    p.send("Enter");

    await p.waitFor("What is this community called?");
    p.type("dogfood");
    p.send("Enter");

    // scrypt runs here — ~1.4 s — then the shell boots against a real daemon
    // this flow just spawned. Waiting on `PLACES` rather than on the
    // statusline: the bands paint before the body settles, so a wait on
    // `buzz://` can return a frame whose body is still blank — which reads as
    // "the home screen is empty" when it is really "the capture was early".
    const shell = await p.waitFor("PLACES", 90_000);
    expect(shell).toContain(RELAY);
    // The relay is unreachable by construction (`.invalid`), so the connection
    // chrome must say so rather than showing a live dot — §1.3 property 3.
    expect(shell).not.toContain("◉ live");
    // And nothing secret survived onto the frame.
    expect(shell).not.toContain(PASSPHRASE);

    // The config the daemon consumes was written, with the identity in it.
    const config = await Bun.file(
      join(machine.root, "state", "buzz", "config.json"),
    ).json();
    expect(config.relayUrl).toBe(RELAY);
    expect(config.communityName).toBe("dogfood");
    expect(config.pubkey).toMatch(/^[0-9a-f]{64}$/);
  }, 180_000);

  test("attach: a running daemon is joined without a passphrase prompt", async () => {
    // §2.3's fast path. The assertion that matters is the *absence* of the
    // prompt: a daemon already running decrypted its key at startup, so asking
    // again would charge every launch of an always-on VPS install (§6.5) for a
    // cold start it never performs.
    if (!daemonBinary) throw new Error("buzz-daemon was not built; cannot run");
    machine = new Machine();

    const provisioned = machine.run(
      ["identity", "provision"],
      JSON.stringify({ mode: "create", passphrase: PASSPHRASE }),
    );
    const pubkey = provisioned.pubkey as string;
    const paths = machine.run([
      "socket-path",
      "--relay",
      RELAY,
      "--identity",
      pubkey,
    ]);

    // Start the daemon out of band, the way `systemd --user` would (§6.5).
    daemonProcess = Bun.spawn(
      [
        daemonBinary,
        "--relay",
        RELAY,
        "--identity",
        pubkey,
        "--passphrase-stdin",
        "--idle-timeout",
        "0",
      ],
      { env: machine.env, stdin: "pipe", stdout: "ignore", stderr: "ignore" },
    );
    daemonProcess.stdin.write(`${PASSPHRASE}\n`);
    await daemonProcess.stdin.end();

    const socket = paths.socket as string;
    for (let i = 0; i < 600 && !existsSync(socket); i++) await Bun.sleep(100);
    expect(existsSync(socket)).toBe(true);

    // Write the config so the TUI takes the attach path rather than onboarding.
    const stateDir = join(machine.root, "state", "buzz");
    await Bun.write(
      join(stateDir, "config.json"),
      JSON.stringify({ relayUrl: RELAY, pubkey, communityName: "attached" }),
    );

    const p = launch(machine);
    const frame = await p.waitFor("PLACES", 60_000);
    expect(frame).toContain(RELAY);
    // No prompt was printed, which is the whole point of probing before asking.
    expect(frame).not.toContain("passphrase for identity");
  }, 180_000);

  test("spawn: an absent daemon is started, and the passphrase goes on stdin", async () => {
    if (!daemonBinary) throw new Error("buzz-daemon was not built; cannot run");
    machine = new Machine();

    const provisioned = machine.run(
      ["identity", "provision"],
      JSON.stringify({ mode: "create", passphrase: PASSPHRASE }),
    );
    const pubkey = provisioned.pubkey as string;
    const paths = machine.run([
      "socket-path",
      "--relay",
      RELAY,
      "--identity",
      pubkey,
    ]);
    // Nothing is listening: this is the cold-start branch.
    expect(existsSync(paths.socket as string)).toBe(false);

    await Bun.write(
      join(machine.root, "state", "buzz", "config.json"),
      JSON.stringify({ relayUrl: RELAY, pubkey, communityName: "spawned" }),
    );

    const p = launch(machine);
    // The prompt appears *because* the socket is dead — the ordering asserted
    // in the attach case above, from the other side.
    await p.waitFor("passphrase for identity", 30_000);
    p.type(PASSPHRASE);
    p.send("Enter");

    // ~1.4 s of scrypt, then a bound socket and a first paint.
    const frame = await p.waitFor("PLACES", 90_000);
    expect(frame).toContain(RELAY);
    expect(existsSync(paths.socket as string)).toBe(true);
    // The passphrase went to the child's stdin; it must not be on the frame.
    expect(frame).not.toContain(PASSPHRASE);
  }, 180_000);

  test("a wrong passphrase fails fast with the daemon's reason, not a timeout", async () => {
    // §2.3: "Spawn failure is a first-class outcome, not a timeout. […] 'Timed
    // out after 5 s' for a bad passphrase is the kind of dead end §1.3
    // property 2 forbids."
    //
    // This is also what makes the 20 s deadline safe: the *child-exit* branch
    // ends the wait, so a wrong passphrase costs one scrypt, not the ceiling.
    if (!daemonBinary) throw new Error("buzz-daemon was not built; cannot run");
    machine = new Machine();

    const provisioned = machine.run(
      ["identity", "provision"],
      JSON.stringify({ mode: "create", passphrase: PASSPHRASE }),
    );
    await Bun.write(
      join(machine.root, "state", "buzz", "config.json"),
      JSON.stringify({
        relayUrl: RELAY,
        pubkey: provisioned.pubkey,
        communityName: "wrong",
      }),
    );

    const p = launch(machine);
    await p.waitFor("passphrase for identity", 30_000);
    p.type("this is not the passphrase");
    p.send("Enter");

    const frame = await p.waitFor("buzz-tui:", 60_000);
    // The daemon's own words reach the operator. "wrong passphrase or damaged
    // key blob" is `identity::decrypt_ncryptsec`'s message, verbatim.
    expect(frame.toLowerCase()).toContain("passphrase");
    expect(frame).not.toContain("timed out");
  }, 180_000);

  /**
   * §2.5/§6.5's client-side socket-directory refusal, end to end.
   *
   * A unit test covers the predicate; this covers the *wiring* — that
   * `BUZZ_DAEMON_SOCKET` consults it before connecting, and that the refusal
   * reaches the operator rather than being swallowed.
   *
   * The second case is the one that matters and it is a regression: a first
   * draft handled the permission failure and the connect failure in one block
   * and chose the message by asking "is the socket live?". A world-writable
   * directory holding a *dead* socket therefore reported "nothing is
   * listening" — the security refusal masked by an unrelated condition.
   */
  test("an unsafe socket directory is refused, and the refusal is not masked", async () => {
    machine = new Machine();
    const forward = join(machine.root, "fwd");
    Bun.spawnSync(["mkdir", "-m", "777", "-p", forward]);
    const socket = join(forward, "x.sock");
    await Bun.write(socket, "");
    // Nothing is listening on it, which is exactly the state that masked the
    // refusal before the fix.

    const p = launch(machine, { BUZZ_DAEMON_SOCKET: socket });
    const frame = await p.waitFor("buzz-tui:", 60_000);
    expect(frame).toContain("group- or world-writable");
    // The remedy, not just the diagnosis.
    expect(frame).toContain("mkdir -p -m 700");
    // And it is not the *other* message.
    expect(frame).not.toContain("nothing is listening");
  }, 90_000);

  test("a dead socket in a safe directory says so, without a stack trace", async () => {
    // Bun surfaces an unreachable UDS as `Was there a typo in the url or
    // port? path: "http://buzz-daemon/health"` — an HTTP URL the operator
    // never typed, naming nothing about the socket that is missing.
    machine = new Machine();
    const dir = join(machine.root, "safe");
    Bun.spawnSync(["mkdir", "-m", "700", "-p", dir]);
    const socket = join(dir, "y.sock");
    await Bun.write(socket, "");

    const p = launch(machine, { BUZZ_DAEMON_SOCKET: socket });
    const frame = await p.waitFor("buzz-tui:", 60_000);
    expect(frame).toContain("nothing is listening");
    expect(frame).toContain("ssh -L");
    expect(frame).not.toContain("typo in the url");
  }, 90_000);

  test("a spawn that fails AFTER onboarding shows its reason on screen", async () => {
    // Found in self-review, and it was invisible in the worst way: the reason
    // went to `console.error` while the renderer owned the terminal, so the
    // operator got a frame reading `setup › done` and `Pane is dead (status
    // 1)` with no cause anywhere. Every other failure path prints *before* the
    // renderer starts, which is why nothing caught this one.
    //
    // Driven with a daemon that provisions correctly and then refuses to bind,
    // because that is exactly the shape of the real cases (`EADDRINUSE`, an
    // unsafe runtime directory) — the identity is written, and only the serve
    // step fails.
    if (!daemonBinary) throw new Error("buzz-daemon was not built; cannot run");
    machine = new Machine();

    const shim = join(machine.root, "refuses-to-serve");
    await Bun.write(
      shim,
      [
        "#!/bin/sh",
        'case "$1" in',
        `  identity|socket-path) exec ${daemonBinary} "$@" ;;`,
        '  *) echo "simulated: cannot bind socket" >&2; exit 1 ;;',
        "esac",
        "",
      ].join("\n"),
    );
    Bun.spawnSync(["chmod", "+x", shim]);

    const p = launch(machine, { BUZZ_DAEMON_BIN: shim });
    await p.waitFor("Welcome to Buzz", 60_000);
    p.send("Enter");
    await p.waitFor("Which relay?");
    p.type(RELAY);
    p.send("Enter");
    await p.waitFor("Create an identity");
    p.send("Enter");
    await p.waitFor("Choose a passphrase");
    p.type(PASSPHRASE);
    p.send("Enter");
    await p.waitFor("Type it again");
    p.type(PASSPHRASE);
    p.send("Enter");
    await p.waitFor("What is this community called?");
    p.type("doomed");
    p.send("Enter");

    // The daemon's own words, and the remedy — the identity is already on
    // disk, so relaunching is the whole fix and re-answering would mint a
    // second identity beside the first.
    const frame = await p.waitFor("cannot bind socket", 120_000);
    expect(frame).toContain("relaunch");
    expect(frame).not.toContain(PASSPHRASE);
  }, 300_000);

  /**
   * The **compiled** binary walks onboarding and spawns a daemon.
   *
   * `scripts/smoke.sh` already proves the artifact renders, but only against
   * `BUZZ_TUI_FIXTURE` — a path that opens no socket and spawns no child. The
   * startup work in this lane added three things a bundler can plausibly break
   * and the fixture path never touches: `fetch(url, { unix })`,
   * `Bun.spawn` with a piped stdin, and `node:fs` on a `0700` directory tree.
   *
   * "It compiled" is not evidence any of those survived, and the failure mode
   * is the one §6.2(b) already warns about for the Solid transform: a binary
   * that builds green and dies at runtime, in a place only a release tag would
   * find it.
   */
  test("the compiled binary onboards and spawns a daemon", async () => {
    if (!daemonBinary) throw new Error("buzz-daemon was not built; cannot run");
    machine = new Machine();
    const p = launchCompiled(machine);
    if (!p) {
      // Skipping loudly rather than silently: a case that no-ops and reports
      // green is worse than none. `bun run build <triple>` produces the input.
      throw new Error(
        "dist/buzz-tui-x86_64-unknown-linux-gnu is absent — run " +
          "`bun run scripts/build.ts x86_64-unknown-linux-gnu` first",
      );
    }

    await p.waitFor("Welcome to Buzz", 60_000);
    p.send("Enter");
    await p.waitFor("Which relay?");
    p.type(RELAY);
    p.send("Enter");
    await p.waitFor("Create an identity");
    p.send("Enter");
    await p.waitFor("Choose a passphrase");
    p.type(PASSPHRASE);
    p.send("Enter");
    await p.waitFor("Type it again");
    p.type(PASSPHRASE);
    p.send("Enter");
    await p.waitFor("What is this community called?");
    p.type("compiled");
    p.send("Enter");

    const frame = await p.waitFor("PLACES", 120_000);
    expect(frame).toContain(RELAY);
    expect(frame).not.toContain(PASSPHRASE);

    // The daemon really started: an identity blob and a bound socket exist.
    const config = await Bun.file(
      join(machine.root, "state", "buzz", "config.json"),
    ).json();
    expect(config.pubkey).toMatch(/^[0-9a-f]{64}$/);
    const paths = machine.run([
      "socket-path",
      "--relay",
      RELAY,
      "--identity",
      config.pubkey,
    ]);
    expect(existsSync(paths.socket as string)).toBe(true);
  }, 300_000);
});
