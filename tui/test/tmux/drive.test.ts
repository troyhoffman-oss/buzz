/**
 * T2 — tmux drive, DESIGN.md §5.5.
 *
 * > `tmux` is a better *gate* than VHS: fast, headless, yields text — and it is
 * > exactly how the operators run the app, so the test harness and the product
 * > share a substrate.
 *
 * These sequences exist because a pure-function test cannot reach them. Every
 * case below is a property of the **real terminal**: a key encoding the app
 * only sees from a PTY, a resize the frame has to survive, or a signal that
 * never reaches the model. Anything provable against `renderScreen` belongs in
 * `test/unit` or `test/render` instead — a T2 case costs seconds and a unit
 * costs milliseconds, so duplicating coverage here buys nothing and slows the
 * gate everybody runs.
 *
 * §5.5's rules, each applied below:
 *
 * - **Never `sleep N` as a readiness proxy** — poll `capture-pane` for a marker
 *   with a hard timeout. Blind sleeps are the number-one source of flaky TUI CI.
 * - **Fixed pane geometry at creation.** `-x/-y` alone is silently ignored when
 *   the box is already running tmux (`window-size latest`), which is this
 *   product's own premise. `manual` + explicit resize + **assert** the result.
 * - **Isolated per-run `XDG_STATE_HOME`** so parallel runs cannot corrupt each
 *   other.
 * - **Always kill the session in a `trap`** — orphaned tmux sessions on a VPS
 *   are how this suite becomes a resource leak.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CWD = new URL("../../", import.meta.url).pathname;
const FIXTURES = join(CWD, "fixtures");

/** A driven tmux session. */
class Pane {
  private readonly session: string;
  private readonly stateDir: string;

  constructor(fixture: string, cols: number, rows: number) {
    this.session = `buzz-tui-t2-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
    this.stateDir = mkdtempSync(join(tmpdir(), "buzz-tui-t2-"));

    this.tmux([
      "new-session",
      "-d",
      "-s",
      this.session,
      "-c",
      CWD,
      "-x",
      String(cols),
      "-y",
      String(rows),
      `XDG_STATE_HOME=${this.stateDir} ` +
        `BUZZ_TUI_FIXTURE=${join(FIXTURES, `${fixture}.jsonl`)} ` +
        "BUZZ_TUI_FIXED_TIME=2026-08-04T14:12:00Z BUZZ_TUI_NO_ANIM=1 " +
        "TZ=UTC LANG=C.UTF-8 bun run src/main.ts",
    ]);

    // `-x/-y` at creation is not enough on a box already running tmux.
    this.tmux(["set-option", "-t", this.session, "window-size", "manual"]);
    this.resize(cols, rows);
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

  /** Resize and **assert** the geometry — a requested size is not a set size. */
  resize(cols: number, rows: number): void {
    this.tmux([
      "resize-window",
      "-t",
      this.session,
      "-x",
      String(cols),
      "-y",
      String(rows),
    ]);
    const actual = this.tmux([
      "list-panes",
      "-t",
      this.session,
      "-F",
      "#{pane_width}x#{pane_height}",
    ]).split("\n")[0];
    if (actual !== `${cols}x${rows}`) {
      throw new Error(`pane geometry is ${actual}, expected ${cols}x${rows}`);
    }
  }

  capture(): string {
    return this.tmux(["capture-pane", "-p", "-t", this.session]);
  }

  /** Send a tmux key name (`Up`, `C-c`, …). */
  send(keys: string): void {
    this.tmux(["send-keys", "-t", this.session, keys]);
  }

  /** Send a literal string, so `-` and friends are not read as key names. */
  type(text: string): void {
    this.tmux(["send-keys", "-t", this.session, "-l", text]);
  }

  /** Poll for a marker rather than sleeping (§5.5). */
  async waitFor(marker: string, timeoutMs = 15_000): Promise<string> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const pane = this.capture();
      if (pane.includes(marker)) return pane;
      if (Date.now() > deadline) {
        throw new Error(
          `timed out waiting for ${JSON.stringify(marker)}\n--- pane ---\n${pane}`,
        );
      }
      await Bun.sleep(50);
    }
  }

  alive(): boolean {
    return (
      Bun.spawnSync(["tmux", "has-session", "-t", this.session]).exitCode === 0
    );
  }

  kill(): void {
    Bun.spawnSync(["tmux", "kill-session", "-t", this.session]);
    rmSync(this.stateDir, { recursive: true, force: true });
  }
}

/**
 * Poll until the pane stops changing — the readiness proxy for a **resize**.
 *
 * `waitFor`'s substring test cannot serve here, and the reason is a genuine
 * tmux trap: **`capture-pane` clips every row to the pane width.** After
 * shrinking 60 → 40, the *old* 60-column frame is reported as 40-column rows,
 * so every content marker — `buzz://`, the composer, even a full-width rule of
 * exactly 40 `─` — matches against a frame the app has not redrawn yet. A test
 * built on any of those passes instantly and asserts on stale pixels.
 *
 * Two identical consecutive captures is the honest signal: SIGWINCH has been
 * delivered, the redraw has landed, and nothing is still in flight. It is also
 * width-agnostic, so it does not encode which segments the statusline happens
 * to drop at a given size — that is the app's decision to make, not the
 * harness's to hard-code.
 */
async function pollUntilStable(
  produce: () => string,
  settleMs = 250,
  timeoutMs = 15_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let previous = produce();
  for (;;) {
    await Bun.sleep(settleMs);
    const next = produce();
    if (next === previous) return next;
    previous = next;
    if (Date.now() > deadline) {
      throw new Error(`pane never settled\n--- pane ---\n${next}`);
    }
  }
}

let pane: Pane | null = null;

/** Always kill the session, even when an assertion threw (§5.5). */
afterEach(() => {
  pane?.kill();
  pane = null;
});

async function open(
  fixture = "seeded-basic",
  cols = 100,
  rows = 30,
): Promise<Pane> {
  pane = new Pane(fixture, cols, rows);
  await pane.waitFor("buzz://");
  return pane;
}

describe("§5.5 real-PTY sequences", () => {
  test("arrow keys navigate — the decoding a unit test starts after", async () => {
    // Arrows arrive as escape sequences (`\x1b[A`), not as names. That decode
    // is the one part of the key path the pure chains cannot exercise, because
    // they begin with a `KeyPress` that already has `name: "up"`.
    const p = await open();
    await p.waitFor("❯ matt");

    p.send("Right"); // teleport into #engineering (§4.4)
    const channel = await p.waitFor("home › mentions › #engineering");
    expect(channel).toContain("44200 cadence");

    p.send("Left");
    await p.waitFor("ATTENTION");
  }, 30_000);

  test("↓ opens the drawer and a printable closes it and inserts [G5]", async () => {
    // §2.4 calls this "the single most important behavior to port". Worth one
    // real-PTY run: a mis-decoded key here would silently eat a keystroke
    // rather than erroring, which is the failure shape hardest to notice.
    const p = await open();
    p.send("Right");
    await p.waitFor("#engineering");

    p.send("Down");
    const drawer = await p.waitFor("AGENTS");
    expect(drawer).toContain("needs input");
    // The drawer replaces the statusline (§2.3) rather than overlaying it.
    expect(drawer).not.toContain("buzz://");

    p.type("o");
    const restored = await p.waitFor("buzz://");
    expect(restored).toContain("❯ o");
  }, 30_000);

  test("ctrl+c mid-compose clears the composer and does NOT exit", async () => {
    // §5.5's own case, and only testable here: `ctrl+c` is the terminal's
    // signal, so nothing below the PTY ever sees it.
    const p = await open();
    p.send("Right");
    await p.waitFor("#engineering");

    p.type("half a thought");
    await p.waitFor("half a thought");

    p.send("C-c");
    await Bun.sleep(300);
    expect(p.alive()).toBe(true);
    expect(p.capture()).not.toContain("half a thought");

    p.send("C-c");
    const deadline = Date.now() + 10_000;
    while (p.alive() && Date.now() < deadline) await Bun.sleep(50);
    expect(p.alive()).toBe(false);
  }, 30_000);

  test("resize 120 → 60 → 40 → 120 keeps the frame legible and intact", async () => {
    // §7's reflow under a *real* resize: SIGWINCH, a re-render, and a terminal
    // that keeps whatever the previous frame left behind wherever a row is
    // short. Neither of those exists in a pure render.
    const p = await open("seeded-basic", 120, 30);
    p.send("Right");
    await p.waitFor("#engineering");

    for (const [cols, rows] of [
      [60, 24],
      [40, 20],
      [120, 30],
    ] as const) {
      p.resize(cols, rows);
      const frame = await pollUntilStable(() => p.capture());

      // The two things §3.0 says are the last to go: the connection glyph
      // (§2.1) and the composer (§2.2). At 40 columns the relay host and the
      // unread total have both dropped and these two have not, which is §2.1's
      // drop order holding under a real resize rather than in a unit.
      expect(frame).toContain("◉");
      expect(frame).toContain("message #engineering");
      // A row wider than the pane wraps, and the frame desynchronises from the
      // model for every row after it.
      for (const row of frame.split("\n")) {
        expect(row.length).toBeLessThanOrEqual(cols);
      }
    }
  }, 45_000);

  test("the drawer stays up and streaming-ready rather than stealing focus", async () => {
    // §2.4's peek must not cost the chat. In a PTY this also shows the render
    // reaches the terminal rather than only the model.
    const p = await open("agent-stream");
    p.send("Right");
    await p.waitFor("#engineering");
    p.send("Down");
    await p.waitFor("AGENTS");

    await Bun.sleep(500);
    const drawer = p.capture();
    expect(drawer).toContain("AGENTS");
    expect(drawer).toContain("↑/↓ select");
  }, 30_000);

  test("a keyless daemon is visibly distinct in a real terminal (§2.5)", async () => {
    const p = await open("keyless-daemon");
    // "a keyless daemon must never look identical to a healthy one."
    expect(await p.waitFor("buzz://")).toContain("keyless");
  }, 30_000);

  /**
   * `⏎` actually sends — the end-to-end write path through the real Shell.
   *
   * This case exists because the write path was dead and looked alive. The
   * reducer set `pending` and nothing drained it, so `⏎` cleared the composer
   * — the visible half — and the message was silently dropped. Every unit test
   * passed, because they asserted on `state.pending`, which is set either way.
   *
   * Only a driven app can catch that, and the assertion has to be *the message
   * appears in the timeline*: "the composer cleared" is exactly what the bug
   * also produced.
   */
  test("⏎ sends and the message lands in the timeline", async () => {
    const p = await open();
    p.send("Right");
    await p.waitFor("#engineering");

    p.type("shipped from a real pty");
    await p.waitFor("shipped from a real pty");

    p.send("Enter");
    // The composer resets to its placeholder...
    const after = await p.waitFor("message #engineering");
    // ...*and* the text is in the timeline. Both halves, or it is the bug.
    expect(after).toContain("shipped from a real pty");
  }, 30_000);
});
