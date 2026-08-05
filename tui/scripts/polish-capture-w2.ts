/**
 * Wave 2 polish capture — the visual lane's test.
 *
 * A polish pass has no unit test. The rows still say the same words, so every
 * assertion in `test/unit` and `test/render` can pass while the screen looks
 * exactly as flat as it did before. **The capture is the evidence**, and this
 * script is what makes it evidence rather than an anecdote: same fixture, same
 * frozen clock, same geometry, driven through a real PTY, so a before/after
 * pair differs only by the change under review.
 *
 * # What this adds over `dogfood-capture.ts`
 *
 * **`capture-pane -e`.** The M-series captures used `-p`, which strips SGR —
 * so they record the text and throw away every colour decision. For a theme
 * pass that is the whole subject, and a capture that drops it cannot show the
 * work. `-e` preserves the escape sequences, so the `.ansi` artifacts below
 * replay in any terminal with `cat` and can be diffed for *styling* changes,
 * not only for text changes.
 *
 * Both forms are written for every frame:
 *
 * - `<id>.txt` — plain text, for reading in a diff and for the geometry
 *   assertions the M-series captures support.
 * - `<id>.ansi` — the same frame with styling, which is the artifact that shows
 *   what actually changed.
 *
 * # Determinism
 *
 * `BUZZ_TUI_FIXED_TIME`, `TZ=UTC`, `LANG=C.UTF-8`, `BUZZ_TUI_NO_ANIM=1` and a
 * fixture transport — §5.3's six requirements, the same set the T1 matrix uses.
 * `BUZZ_TUI_THEME=dark` is pinned as well: the palette is resolved from the
 * environment once at startup, and a capture taken under a terminal that
 * happened to report `COLORFGBG` would record Latte on one box and Macchiato on
 * the next, which would make the artifacts incomparable for the exact property
 * they exist to demonstrate.
 *
 * Usage:
 *   bun run scripts/polish-capture-w2.ts <outdir> [--binary <path>]
 */

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CWD = new URL("../", import.meta.url).pathname;
const FIXTURES = join(CWD, "fixtures");

/**
 * A driven tmux session — the §5.5 `Pane` pattern.
 *
 * Readiness is **observed, never assumed**: every step polls `capture-pane` for
 * a marker with a hard timeout. `test/tmux/README.md` names blind sleeps the
 * number-one source of flaky TUI CI, and under the load this box runs at that
 * is not a theoretical concern — it is the documented failure mode of this
 * lane's own suites.
 */
class Pane {
  private readonly session: string;
  private readonly stateDir: string;

  constructor(
    fixture: string,
    cols: number,
    rows: number,
    binary: string | undefined,
  ) {
    this.session = `buzz-w2-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
    this.stateDir = join(
      tmpdir(),
      `buzz-w2-${Math.random().toString(36).slice(2, 8)}`,
    );
    mkdirSync(this.stateDir, { recursive: true });

    // The compiled binary when one is given, the entry point otherwise. The
    // binary is what an operator runs, so it is what a polish capture should
    // show; `bun run src/main.ts` is the fallback that needs no build.
    const command = binary ?? "bun run src/main.ts";

    // **The compiled binary must not be launched from `tui/`.** It re-reads
    // whatever `bunfig.toml` sits in its CWD, and that file declares
    // `preload = ["@opentui/solid/preload"]` for dev and test — which the
    // binary's embedded module graph has no way to resolve, so it dies on
    // startup with `preload not found` even though the Solid transform is
    // already baked in. `bunfig.toml` documents this and `scripts/smoke.sh`
    // launches from a scratch directory for the same reason.
    //
    // The interpreted path is the opposite: it *needs* that preload, so it has
    // to run from the package root. One flag, two correct answers.
    const launchCwd = binary ? this.stateDir : CWD;

    this.tmux([
      "new-session",
      "-d",
      "-s",
      this.session,
      "-c",
      launchCwd,
      "-x",
      String(cols),
      "-y",
      String(rows),
      `XDG_STATE_HOME=${this.stateDir} ` +
        `BUZZ_TUI_FIXTURE=${join(FIXTURES, `${fixture}.jsonl`)} ` +
        "BUZZ_TUI_FIXED_TIME=2026-08-04T14:12:00Z BUZZ_TUI_NO_ANIM=1 " +
        "BUZZ_TUI_THEME=dark TZ=UTC LANG=C.UTF-8 " +
        `${command}; sleep 600`,
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

  /** The frame as plain text. */
  capture(): string {
    return this.tmux(["capture-pane", "-p", "-t", this.session]);
  }

  /** The frame **with styling** — `-e` keeps the SGR sequences (§5.5). */
  captureStyled(): string {
    return this.tmux(["capture-pane", "-p", "-e", "-t", this.session]);
  }

  send(keys: string): void {
    this.tmux(["send-keys", "-t", this.session, keys]);
  }

  type(text: string): void {
    this.tmux(["send-keys", "-t", this.session, "-l", text]);
  }

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

  /**
   * Poll until the frame stops changing — the settle rule, load-tolerant.
   *
   * `waitFor` proves the marker arrived; it does not prove the rest of the
   * frame finished painting, and a frame caught mid-redraw is never what should
   * be written. The M-series spent a fixed 300–400 ms here, which is a guess
   * about how loaded the box is — the same guess that makes this lane's suites
   * fail spuriously above load 50. Two identical consecutive captures is the
   * honest signal, and it returns immediately when the box is quiet.
   */
  async settle(quietMs = 120, timeoutMs = 20_000): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    let previous = this.captureStyled();
    for (;;) {
      await Bun.sleep(quietMs);
      const next = this.captureStyled();
      if (next === previous) return;
      previous = next;
      if (Date.now() > deadline) return;
    }
  }

  kill(): void {
    Bun.spawnSync(["tmux", "kill-session", "-t", this.session]);
    rmSync(this.stateDir, { recursive: true, force: true });
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
  readonly fixture: string;
  readonly steps: readonly Step[];
}

/**
 * The scenarios.
 *
 * The seven M-series layers, so the before/after pairs line up one-for-one —
 * plus the three states the M-series never captured at all. Wave 2's brief
 * names empty / loading / connecting explicitly ("every state distinguishable
 * and calm"), and those are exactly the screens with no rows on them, which is
 * where a flat renderer is least defensible and easiest to leave unexamined.
 */
const SCENARIOS: readonly Scenario[] = [
  {
    id: "01-home",
    title: "L0 HOME — attention groups, community, places",
    fixture: "seeded-basic",
    steps: [
      {
        key: "(boot)",
        expect: "ATTENTION",
        note: "§1.1 default selection lands on the top attention row at boot",
      },
    ],
  },
  {
    id: "02-channels",
    title: "L1 CHANNELS — the list, and the filter that demotes its cursor",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "⇧↓",
        send: "S-Down",
        expect: "❯ ▾ THREADS",
        note: "leap past MENTIONS",
      },
      {
        key: "⇧↓",
        send: "S-Down",
        expect: "❯ ▾ NEEDS ACTION",
        note: "past THREADS",
      },
      {
        key: "⇧↓",
        send: "S-Down",
        expect: "❯ ▾ DMs",
        note: "past NEEDS ACTION",
      },
      {
        key: "⇧↓",
        send: "S-Down",
        expect: "❯ PLACES",
        note: "onto the PLACES header",
      },
      {
        key: "↓",
        send: "Down",
        expect: "❯ Channels",
        note: "step onto Channels",
      },
      {
        key: "→",
        send: "Right",
        expect: "home › channels",
        note: "descend to L1",
      },
    ],
  },
  {
    id: "03-channels-filtered",
    title: "L1 CHANNELS — one typed character moves ❯ and demotes the row to ▌",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "⇧↓", send: "S-Down", expect: "❯ ▾ THREADS", note: "leap" },
      { key: "⇧↓", send: "S-Down", expect: "❯ ▾ NEEDS ACTION", note: "leap" },
      { key: "⇧↓", send: "S-Down", expect: "❯ ▾ DMs", note: "leap" },
      { key: "⇧↓", send: "S-Down", expect: "❯ PLACES", note: "leap" },
      { key: "↓", send: "Down", expect: "❯ Channels", note: "step" },
      { key: "→", send: "Right", expect: "home › channels", note: "descend" },
      {
        key: "type 'eng'",
        type: "eng",
        expect: "▌ #engineering",
        note: "§2.2 — the composer takes ❯, the row keeps ▌ as its position",
      },
    ],
  },
  {
    id: "04-timeline",
    title: "L2 TIMELINE — day divider, author grouping, thread counts",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "#engineering",
        note: "§4.4 two-keystroke teleport into the mentioned channel",
      },
    ],
  },
  {
    id: "05-message-select",
    title: "L2 MESSAGE-SELECT — ↑ enters, the composer is replaced by hints",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      {
        key: "↑",
        send: "Up",
        expect: "↑/↓ message",
        note: "§5.2 — selection enters the timeline",
      },
    ],
  },
  {
    id: "06-thread",
    title: "L3 THREAD — ⇧↑ leaps thread-root to thread-root, → descends",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      { key: "↑", send: "Up", expect: "↑/↓ message", note: "message-select" },
      {
        key: "⇧↑",
        send: "S-Up",
        expect: "❯ @troy the 44200",
        note: "thread-leap",
      },
      {
        key: "→",
        send: "Right",
        expect: "reply in thread",
        note: "descend to L3",
      },
    ],
  },
  {
    id: "07-mention-picker",
    title: "MENTION PICKER — renders above the top rule, composer stays live",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      {
        key: "type '@'",
        type: "@",
        expect: "⏎/⇥ insert",
        note: "§3.3 — §2.5's completion band, a different class from the drawer",
      },
    ],
  },
  {
    id: "08-drawer",
    title: "DRAWER — ↓ replaces composer + statusline with the live ladder",
    fixture: "agent-stream",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      {
        key: "↓",
        send: "Down",
        expect: "↑/↓ select",
        note: "§2.3 — the drawer replaces the two bottom bands, never overlays",
      },
    ],
  },
  {
    id: "09-search",
    title: "SEARCH — ctrl+k, results as you type",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      {
        key: "ctrl+k",
        send: "C-k",
        expect: "› search",
        note: "§3.5 search layer",
      },
      {
        key: "type 'read'",
        type: "read",
        expect: "read-state slots cap",
        note: "hits render as the query is typed",
      },
    ],
  },
  {
    id: "10-agents",
    title: "L1 AGENTS — the fleet, sorted blocked-first",
    fixture: "agent-stream",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "⇧↓", send: "S-Down", expect: "❯ ▾ THREADS", note: "leap" },
      { key: "⇧↓", send: "S-Down", expect: "❯ ▾ NEEDS ACTION", note: "leap" },
      { key: "⇧↓", send: "S-Down", expect: "❯ ▾ DMs", note: "leap" },
      { key: "⇧↓", send: "S-Down", expect: "❯ PLACES", note: "leap" },
      { key: "↓", send: "Down", expect: "❯ Channels", note: "step" },
      { key: "↓", send: "Down", expect: "❯ Agents", note: "step onto Agents" },
      { key: "→", send: "Right", expect: "home › agents", note: "descend" },
    ],
  },
  // The three states the M-series never captured. A blank pane is where a flat
  // renderer is least defensible, and §1.3 property 3 makes it the highest-stakes
  // screen in the product: "a chat client that *looks* idle while its socket is
  // dead is the worst failure mode".
  {
    id: "11-empty-cold",
    title: "EMPTY — cold start, nothing ingested yet",
    fixture: "empty",
    steps: [
      {
        key: "(boot)",
        expect: "PLACES",
        note: "no communities, no attention — home still explains itself",
      },
    ],
  },
  {
    id: "12-keyless",
    title: "KEYLESS — a daemon with no identity must never look healthy",
    fixture: "keyless-daemon",
    steps: [
      {
        key: "(boot)",
        expect: "⚠ keyless",
        note: "§2.5 — archiving:false is a loss state and is rendered as one",
      },
    ],
  },
];

// A `reconnect` scenario was drafted here and removed rather than left flaky.
// The fixture transitions to `reconnecting` at 500 ms and back to `connected`
// at 2000 ms, so the degraded frame exists for a second and a half — a window
// this harness cannot land in deterministically, because `settle()` waits for
// the pane to *stop changing* and the pane is mid-recovery for exactly that
// span. A capture that sometimes shows `◌ retry 2` and sometimes `◉ live` is
// not evidence of anything.
//
// The state is covered where it can be asserted rather than raced:
// `test/unit/render-parts.test.ts` pins that every non-connected state gets a
// distinct glyph (§2.6), and `test/unit/emptystate.test.ts` pins that each one
// produces its own reason string. Both are stronger than a screenshot; what a
// capture would add is the *colour*, and `connectionStyle` is a total function
// over the same eight states with three tones, which the keyless frame above
// already demonstrates on its `⚠ keyless` segment.

/**
 * The two widths, co-equal.
 *
 * DESIGN §3.9's owner directive is explicit that these are not a primary and a
 * fallback — "wide tiers get the full three-pane layout designed to their
 * width, not a stretched phone layout", and dogfood artifacts "must capture
 * BOTH". A capture at one width proves nothing about the other, and §2.1's drop
 * order is only visible in the pair.
 */
const WIDTHS = [
  { cols: 120, rows: 36, label: "120col" },
  { cols: 60, rows: 30, label: "60col" },
] as const;

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const outdir = args[0];
  const binaryFlag = args.indexOf("--binary");
  const binary = binaryFlag >= 0 ? args[binaryFlag + 1] : undefined;
  if (!outdir) {
    console.error(
      "usage: bun run scripts/polish-capture-w2.ts <outdir> [--binary <path>]",
    );
    process.exit(1);
  }

  const transcript: string[] = [
    "# Buzz TUI — Wave 2 polish captures",
    "",
    "Every frame below came from a real tmux PTY driving " +
      (binary
        ? `the compiled binary (\`${binary.split("/").pop()}\`)`
        : "`bun run src/main.ts`") +
      " against a fixture transport, at a frozen clock, with the dark",
    "(Catppuccin Macchiato) theme pinned.",
    "",
    "Each frame is written twice: `.txt` (plain, for diffing text) and `.ansi`",
    "(styled, via `capture-pane -e`, which is the artifact that shows the",
    "theme). `cat` an `.ansi` file in any terminal to replay it.",
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

      const pane = new Pane(scenario.fixture, width.cols, width.rows, binary);
      try {
        const keylog: string[] = [];
        for (const step of scenario.steps) {
          if (step.send) pane.send(step.send);
          if (step.type) pane.type(step.type);
          await pane.waitFor(step.expect);
          keylog.push(`${step.key.padEnd(12)} → ${step.note}`);
        }
        await pane.settle();

        const plain = pane.capture();
        const styled = pane.captureStyled();

        const header =
          `${scenario.title}\n` +
          `geometry: ${width.cols}x${width.rows}\n` +
          `fixture:  ${scenario.fixture}\n` +
          `theme:    catppuccin-macchiato (dark, pinned)\n` +
          `keys:\n${keylog.map((k) => `  ${k}`).join("\n")}\n` +
          `${"─".repeat(Math.min(width.cols, 100))}\n`;

        writeFileSync(join(dir, `${scenario.id}.txt`), `${header}${plain}`);
        writeFileSync(join(dir, `${scenario.id}.ansi`), styled);
        console.log(`wrote ${join(dir, `${scenario.id}.{txt,ansi}`)}`);

        transcript.push(
          `### ${width.cols}×${width.rows}`,
          "",
          "Keys:",
          "",
          ...keylog.map((k) => `- \`${k}\``),
          "",
          "```",
          plain.replace(/\s+$/, ""),
          "```",
          "",
        );
      } finally {
        // Always, even when a step threw. A leaked tmux session running a TUI
        // is a busy-loop on someone else's box — this lane has already cost
        // 551 CPU-minutes to exactly that mistake.
        pane.kill();
      }
    }
  }

  writeFileSync(join(outdir, "WALKTHROUGH.md"), transcript.join("\n"));
  console.log(`wrote ${join(outdir, "WALKTHROUGH.md")}`);
}

await main();
