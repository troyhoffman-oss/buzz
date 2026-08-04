/**
 * Dogfood capture harness — drives the shipped TUI in a real tmux PTY and
 * writes one annotated capture per layer, at two widths.
 *
 * This is the §5.5 `Pane` pattern from `test/tmux/drive.test.ts` reused as a
 * *recorder* rather than a gate: same readiness rules (poll for a marker, never
 * `sleep N`; assert the geometry after `window-size manual`; kill the session in
 * a finally), but every step also emits the keystroke that produced it, so the
 * transcript reads as a walkthrough rather than a pile of screenshots.
 *
 * Usage:
 *   bun run scripts/dogfood-capture.ts <outdir> [fixture]
 *
 * Widths are paired deliberately: 120 columns is the comfortable case and 60 is
 * the §7 reflow case. A capture at only one width proves nothing about the
 * other, and §2.1's drop order is only visible in the pair.
 */

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CWD = new URL("../", import.meta.url).pathname;
const FIXTURES = join(CWD, "fixtures");

/** A driven tmux session — the harness of `test/tmux/drive.test.ts` §5.5. */
class Pane {
  private readonly session: string;
  private readonly stateDir: string;

  constructor(fixture: string, cols: number, rows: number) {
    this.session = `buzz-dogfood-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
    this.stateDir = join(
      tmpdir(),
      `buzz-dogfood-${Math.random().toString(36).slice(2, 8)}`,
    );
    mkdirSync(this.stateDir, { recursive: true });

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

    // `-x/-y` at creation is silently ignored on a box already running tmux.
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

  async waitFor(marker: string, timeoutMs = 20_000): Promise<string> {
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
    Bun.spawnSync(["tmux", "kill-session", "-t", this.session]);
    rmSync(this.stateDir, { recursive: true, force: true });
  }
}

/** One step of a walkthrough: a key to press and the marker it should produce. */
interface Step {
  /** Human-readable keystroke, e.g. `→` or `ctrl+k`. */
  readonly key: string;
  /** What to do — `send` a tmux key name, or `type` a literal. */
  readonly send?: string;
  readonly type?: string;
  /** Marker to wait for after the press. */
  readonly expect: string;
  /** What this step is demonstrating. */
  readonly note: string;
}

/** A named layer capture: a sequence of steps ending at the layer of interest. */
interface Scenario {
  readonly id: string;
  readonly title: string;
  readonly fixture: string;
  readonly steps: readonly Step[];
}

const SCENARIOS: readonly Scenario[] = [
  {
    id: "01-home",
    title: "L0 HOME — attention groups, channels, agents",
    fixture: "seeded-basic",
    steps: [
      {
        key: "(boot)",
        expect: "ATTENTION",
        note: "§1.1 default selection applied at boot",
      },
    ],
  },
  {
    id: "02-channels",
    title: "L1 CHANNELS — fuzzy filter over the channel list",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      // Four leaps, not one: `⇧↓` steps ONE structural boundary, and home has
      // four ATTENTION groups above PLACES. The count is fixture-derived — see
      // the note in the manifest about why this is not hardcoded blindly.
      {
        key: "⇧↓ ×4",
        send: "S-Down",
        expect: "❯ ▾ THREADS",
        note: "§4.1 leap past MENTIONS",
      },
      {
        key: "",
        send: "S-Down",
        expect: "❯ ▾ NEEDS ACTION",
        note: "past THREADS",
      },
      { key: "", send: "S-Down", expect: "❯ ▾ DMs", note: "past NEEDS ACTION" },
      {
        key: "",
        send: "S-Down",
        expect: "❯ PLACES",
        note: "lands on the PLACES header",
      },
      {
        key: "↓",
        send: "Down",
        expect: "❯ Channels",
        note: "step onto the Channels row",
      },
      {
        key: "→",
        send: "Right",
        expect: "home › channels",
        note: "descend into L1",
      },
      // `▌`, not `❯`: one typed character moves `❯` to the composer and demotes
      // the selected row to `▌` (NAVIGATION.md §2.2, `layers/channels.ts:240`).
      // Asserting `❯ #engineering` here would be asserting a bug.
      {
        key: "type 'eng'",
        type: "eng",
        expect: "▌ #engineering",
        note: "§3.2 fuzzy filter narrows the list to one",
      },
    ],
  },
  {
    id: "03-timeline",
    title: "L2 TIMELINE — message rendering and the composer",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "#engineering",
        note: "§4.4 two-keystroke teleport to the mention",
      },
    ],
  },
  {
    id: "04-thread",
    title: "L3 THREAD — ⇧↑ thread-leap then → descends",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "#engineering",
        note: "teleport into the channel",
      },
      // `↑` first: the composer holds focus on entry, and `↑` is what moves
      // into message-select. `⇧↑` from the composer would not leap.
      {
        key: "↑",
        send: "Up",
        expect: "↑/↓ message",
        note: "§5.2 enter message-select",
      },
      {
        key: "⇧↑",
        send: "S-Up",
        expect: "❯ @troy the 44200",
        note: "thread-leap to the previous thread root",
      },
      {
        key: "→",
        send: "Right",
        expect: "reply in thread",
        note: "descend into L3",
      },
    ],
  },
  {
    id: "05-mention-picker",
    title: "MENTION PICKER — @ lists the resolved directory ([D-2])",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "#engineering",
        note: "teleport into the channel",
      },
      {
        key: "type '@'",
        type: "@",
        expect: "⏎/⇥ insert",
        note: "§3.3 picker opens on the composer's @",
      },
    ],
  },
  {
    id: "06-drawer",
    title: "DRAWER PEEK — ↓ shows agent activity without stealing focus",
    fixture: "agent-stream",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "#engineering",
        note: "teleport into the channel",
      },
      {
        key: "↓",
        send: "Down",
        expect: "↑/↓ select",
        note: "§2.4 drawer peek replaces the statusline",
      },
    ],
  },
  {
    id: "07-search",
    title: "SEARCH — ctrl+k opens the results layer",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "#engineering",
        note: "teleport into the channel",
      },
      {
        key: "ctrl+k",
        send: "C-k",
        expect: "› search",
        note: "§3.5 search layer opens",
      },
      {
        key: "type 'read'",
        type: "read",
        expect: "read-state slots cap",
        note: "hits render as the query is typed",
      },
    ],
  },
];

const WIDTHS = [
  { cols: 120, rows: 36, label: "120col" },
  { cols: 60, rows: 30, label: "60col" },
] as const;

async function main(): Promise<void> {
  const outdir = process.argv[2];
  if (!outdir) {
    console.error(
      "usage: bun run scripts/dogfood-capture.ts <outdir> [fixture]",
    );
    process.exit(1);
  }

  const transcript: string[] = [
    "# Buzz TUI — dogfood walkthrough (M1)",
    "",
    "Every frame below was captured from a real tmux PTY driving the shipped",
    "entry point (`bun run src/main.ts`), not from a render unit test. Each",
    "block names the keystroke that produced it.",
    "",
    "**Transport: `BUZZ_TUI_FIXTURE`.** The real daemon serves empty collections",
    "on every mounted endpoint (see `live-daemon/`), so a live capture would show",
    "empty layers and prove nothing about the rendering. The fixture transport is",
    "a product path (§5.4), and it is the same binary either way.",
    "",
  ];

  for (const scenario of SCENARIOS) {
    transcript.push(
      `## ${scenario.title}`,
      "",
      `Fixture: \`${scenario.fixture}\``,
      "",
    );
    for (const width of WIDTHS) {
      const dir = join(outdir, `captures-${width.label}`);
      mkdirSync(dir, { recursive: true });

      const pane = new Pane(scenario.fixture, width.cols, width.rows);
      try {
        let frame = "";
        const keylog: string[] = [];
        for (const step of scenario.steps) {
          if (step.send) pane.send(step.send);
          if (step.type) pane.type(step.type);
          frame = await pane.waitFor(step.expect);
          keylog.push(`${step.key.padEnd(12)} → ${step.note}`);
        }
        // One settle pass so a frame mid-redraw is never what gets written.
        await Bun.sleep(300);
        frame = pane.capture();

        const header =
          `${scenario.title}\n` +
          `geometry: ${width.cols}x${width.rows}\n` +
          `fixture:  ${scenario.fixture}\n` +
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
