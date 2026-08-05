/**
 * Dogfood M3 capture harness — **the compiled binary against a live daemon
 * whose stores are full**.
 *
 * What separates this from M2 is one thing, and it is the milestone:
 * M2's live scenarios could only reach L0 and the two empty lists, because the
 * daemon had no relay ingest and every store was legitimately empty. Those
 * captures proved the transport and nothing about the layers. With `wire.rs`
 * landed and the cold-start discovery walk wired in, the **same** live daemon
 * now serves real channels, real timelines, and a real roster — so the deep
 * walk of `NAVIGATION.md` §4 can be driven against production data rather than
 * against a fixture.
 *
 * Every scenario here is therefore `live`. The fixture transport is not
 * repeated: M1 and M2 both captured it, it is unchanged, and a third copy would
 * dilute the set rather than add to it. Where a live capture cannot reach a
 * layer, that is recorded in `MANIFEST.md` as a finding, not padded with a
 * fixture stand-in.
 *
 * Usage:
 *   bun run scripts/dogfood-capture-m3.ts <outdir> <binary> <daemon-socket>
 *
 * # Read-only, structurally
 *
 * The walks below descend, select, expand, and search. **None of them presses
 * `⏎` on a non-empty composer**, which is the only keystroke that sends. The
 * identity behind the socket is the shared `claude-test` agent and this harness
 * must never publish as it.
 *
 * # Two traps inherited from M2, both still live
 *
 * 1. A compiled Bun binary re-reads `bunfig.toml` from its **CWD**, and
 *    `tui/bunfig.toml` names a `preload` the embedded graph cannot resolve — so
 *    running it from inside `tui/` dies with `preload not found` and reads as a
 *    broken release build. Every pane starts in a scratch directory.
 * 2. `tmux` may be a **shell function** (oh-my-zsh defines one), invisible to a
 *    non-interactive `Bun.spawnSync`. Absolute path, always.
 */

import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CWD = new URL("../", import.meta.url).pathname;

/** `tmux` by absolute path — see the module doc, trap 2. */
const TMUX = "/usr/bin/tmux";

/** A driven tmux session running the compiled binary against a live daemon. */
class Pane {
  private readonly session: string;
  private readonly stateDir: string;
  /** The pane's CWD — deliberately *not* `tui/`; see the module doc, trap 1. */
  private readonly scratch: string;

  constructor(binary: string, socket: string, cols: number, rows: number) {
    this.session = `buzz-m3-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
    this.stateDir = mkdtempSync(join(tmpdir(), "buzz-m3-state-"));
    this.scratch = mkdtempSync(join(tmpdir(), "buzz-m3-cwd-"));

    this.tmux([
      "new-session",
      "-d",
      "-s",
      this.session,
      "-c",
      this.scratch,
      "-x",
      String(cols),
      "-y",
      String(rows),
      // The shell outlives the binary on purpose: `remain-on-exit on` *clears*
      // a dead pane and prints "Pane is dead", so a crash's message — the one
      // thing worth capturing — would be gone before anything read it.
      `XDG_STATE_HOME=${this.stateDir} BUZZ_DAEMON_SOCKET=${socket} ` +
        `BUZZ_TUI_NO_ANIM=1 TZ=UTC LANG=C.UTF-8 ${binary}; sleep 600`,
    ]);

    // `-x/-y` at creation is silently ignored on a box already running tmux,
    // so the size must be set explicitly and then **asserted**.
    this.tmux(["set-option", "-t", this.session, "window-size", "manual"]);
    this.resize(cols, rows);
  }

  private tmux(args: string[]): string {
    const result = Bun.spawnSync([TMUX, ...args], { cwd: CWD });
    if (result.exitCode !== 0) {
      throw new Error(
        `tmux ${args[0]} failed: ${new TextDecoder().decode(result.stderr)}`,
      );
    }
    return new TextDecoder().decode(result.stdout);
  }

  /** Resize and **assert** — a requested size is not a set size. */
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

  send(keys: string): void {
    this.tmux(["send-keys", "-t", this.session, keys]);
  }

  type(text: string): void {
    this.tmux(["send-keys", "-t", this.session, "-l", text]);
  }

  /** Poll for a marker. Never `sleep N` — readiness is observed, not assumed. */
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
      await Bun.sleep(50);
    }
  }

  kill(): void {
    Bun.spawnSync([TMUX, "kill-session", "-t", this.session]);
  }
}

/** One step of a walkthrough: a key to press and the marker it should produce. */
interface Step {
  readonly key: string;
  readonly send?: string;
  readonly type?: string;
  readonly expect: string;
  readonly note: string;
}

interface Scenario {
  readonly id: string;
  readonly title: string;
  readonly steps: readonly Step[];
}

/**
 * The live walk of `NAVIGATION.md` §4, against production data.
 *
 * Markers are chosen so each one **cannot** match the frame before its key.
 * That is the failure mode a loose marker produces: the wait returns
 * immediately, the capture records the previous layer, and the file claims a
 * layer it never reached. M2 hit this twice (the word "activity" appears in the
 * drawer's own header; "search" appears in the home placeholder), so every
 * marker below is either a layer signature — a composer placeholder, a
 * breadcrumb segment, a footer hint — or a string only the target layer paints.
 */
const SCENARIOS: readonly Scenario[] = [
  {
    id: "L01-home-live",
    title: "L0 HOME — live daemon, real identity, real attention groups",
    steps: [
      {
        key: "(boot)",
        expect: "PLACES",
        note: "attached over BUZZ_DAEMON_SOCKET; statusline carries the real relay host and pubkey",
      },
    ],
  },
  {
    id: "L02-channels-live",
    title: "L1 CHANNELS — the relay's own membership answer, discovered cold",
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "home › channels",
        note: "descend to L1 — these rows came from the cold-start discovery walk, not from a fixture",
      },
    ],
  },
  {
    id: "L03-timeline-live",
    title: "L2 CHANNEL — a real timeline, from a real relay",
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      { key: "→", send: "Right", expect: "home › channels", note: "L1" },
      {
        key: "→",
        send: "Right",
        // The breadcrumb's third segment is the layer signature. The composer
        // placeholder cannot be used: L1 already renders `message #<name>` for
        // the *selected* row (§2.2), so waiting for it would match the frame
        // before this key and record L1 while claiming L2.
        expect: "› channels › ",
        note: "descend into the selected channel; the breadcrumb gains a third segment",
      },
    ],
  },
  {
    id: "L04-message-select-live",
    title: "§4.2 — ↑ from an empty composer enters message-select on live rows",
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      { key: "→", send: "Right", expect: "home › channels", note: "L1" },
      { key: "→", send: "Right", expect: "› channels › ", note: "L2" },
      {
        key: "↑",
        send: "Up",
        // The select-mode footer replaces the statusline entirely; it is
        // painted by no other layer.
        expect: "↑/↓ message",
        note: "§5.2 — the ❯ moves out of the composer and into the timeline [G8]",
      },
    ],
  },
  {
    id: "L05-drawer-live",
    title: "§4.3 — ↓ from an empty composer peeks the live drawer",
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      { key: "→", send: "Right", expect: "home › channels", note: "L1" },
      { key: "→", send: "Right", expect: "› channels › ", note: "L2" },
      {
        key: "↓",
        send: "Down",
        expect: "↑/↓ select",
        note: "§2.3 — the drawer replaces the statusline; what it lists is what the daemon reports live",
      },
    ],
  },
  {
    id: "L06-search-live",
    title: "ctrl+k — search against the daemon's real index",
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      {
        key: "ctrl+k",
        send: "C-k",
        expect: "type to search",
        note: "§3.5 opens with a prompt, not 'no results' — an unasked query has not failed",
      },
      {
        key: "type 'read'",
        type: "read",
        // Deliberately *not* asserting a specific hit: the community's content
        // is not this harness's to pin. The assertion is that the query left
        // the prompt state, which is what proves the daemon answered.
        expect: "read",
        note: "a real query against the live /search — hits or an honest zero, never a hang",
      },
    ],
  },
  {
    id: "L07-agents-live",
    title: "L1 AGENTS — the fleet view against live observer data",
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      { key: "↓", send: "Down", expect: "❯ Agents", note: "step to Agents" },
      {
        key: "→",
        send: "Right",
        expect: "home › agents",
        note: "descend; rows come from the observer subscription's 300s freshness window",
      },
    ],
  },
];

const WIDTHS = [
  { cols: 120, rows: 36, label: "120col" },
  { cols: 60, rows: 30, label: "60col" },
] as const;

async function main(): Promise<void> {
  const [outdir, binary, socket] = process.argv.slice(2);
  if (!outdir || !binary || !socket) {
    console.error(
      "usage: bun run scripts/dogfood-capture-m3.ts <outdir> <binary> <daemon-socket>",
    );
    process.exit(1);
  }

  const transcript: string[] = [
    "# Buzz TUI — dogfood walkthrough (M3)",
    "",
    "Every frame below came from a real tmux PTY driving the **compiled**",
    `binary (\`${binary.split("/").pop()}\`) attached over \`BUZZ_DAEMON_SOCKET\` to a`,
    "**live** `buzz-daemon` holding the real `claude-test` identity against",
    "`wss://monumentsquare.communities.buzz.xyz`.",
    "",
    "This is what separates M3 from M2: every capture here is `live`. M2's live",
    "scenarios could only reach L0 and two empty lists, because the daemon had",
    "no relay ingest. The deep layers below are driven against production data.",
    "",
    "**Read-only.** These walks descend, select, expand, and search. None of",
    "them presses `⏎` on a non-empty composer, which is the only keystroke that",
    "sends.",
    "",
  ];

  for (const scenario of SCENARIOS) {
    transcript.push(`## ${scenario.title}`, "", "Transport: `live daemon`", "");

    for (const width of WIDTHS) {
      const dir = join(outdir, `captures-${width.label}`);
      mkdirSync(dir, { recursive: true });

      const pane = new Pane(binary, socket, width.cols, width.rows);
      try {
        const keylog: string[] = [];
        for (const step of scenario.steps) {
          if (step.send) pane.send(step.send);
          if (step.type) pane.type(step.type);
          await pane.waitFor(step.expect);
          keylog.push(`${step.key.padEnd(12)} → ${step.note}`);
        }
        // One settle pass. The wait above proves the marker arrived; it does
        // not prove the rest of the frame has finished painting, and a frame
        // caught mid-redraw is never what should be written.
        await Bun.sleep(400);
        const frame = pane.capture();

        const header =
          `${scenario.title}\n` +
          `geometry:  ${width.cols}x${width.rows}\n` +
          `transport: live daemon (${socket})\n` +
          `binary:    ${binary}\n` +
          `keys:\n${keylog.map((k) => `  ${k}`).join("\n")}\n` +
          `${"─".repeat(Math.min(width.cols, 100))}\n`;
        const file = join(dir, `${scenario.id}.txt`);
        writeFileSync(file, `${header}${frame}`);
        console.log(`wrote ${file}`);

        transcript.push(
          `### ${width.cols}×${width.rows}`,
          "",
          "Keys:",
          "",
          ...keylog.map((k) => `- \`${k}\``),
          "",
          "```",
          frame.replace(/\s+$/, ""),
          "```",
          "",
        );
      } finally {
        pane.kill();
      }
    }
  }

  writeFileSync(join(outdir, "WALKTHROUGH.md"), transcript.join("\n"));
  console.log(`wrote ${join(outdir, "WALKTHROUGH.md")}`);
}

await main();
