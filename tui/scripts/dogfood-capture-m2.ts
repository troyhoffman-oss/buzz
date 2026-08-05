/**
 * Dogfood M2 capture harness — the **compiled binary**, at two widths, against
 * both a live daemon and the fixture transport.
 *
 * Two things make this different from `dogfood-capture.ts` (M1), and both are
 * the point of the milestone:
 *
 * 1. **It drives `dist/buzz-tui-<triple>`, not `bun run src/main.ts`.** M1
 *    captured the source entry point. A compiled Bun binary is a materially
 *    different artifact — it carries its own module graph, applies the Solid
 *    transform at build time rather than through `bunfig.toml`'s preload, and
 *    is what a user actually installs. Capturing the source path and shipping
 *    the compiled one means the screenshots describe a program nobody runs.
 * 2. **It can attach to a real daemon over `BUZZ_DAEMON_SOCKET`**, so the
 *    frames show real session data — the true relay host and pubkey — rather
 *    than fixture values.
 *
 * # Why there are still fixture captures
 *
 * The daemon's relay ingest does not exist yet (`daemon-wire-notes.md` §Status,
 * every box unchecked), so a live daemon serves empty collections on every
 * mounted endpoint and 404s the rest. L2 TIMELINE, L3 THREAD, and L4 ACTIVITY
 * have nothing to render against it. Capturing only the live path would produce
 * a set of empty panes that prove nothing about the layers; capturing only the
 * fixture path would repeat M1 and hide the gap.
 *
 * So both are captured and the manifest diffs them. The `live` scenarios are
 * the honest state of the integration; the `fixture` scenarios are the
 * rendering, through the same compiled binary.
 *
 * Usage:
 *   bun run scripts/dogfood-capture-m2.ts <outdir> <binary> [daemon-socket]
 *
 * # A trap that costs an hour if you meet it cold
 *
 * A compiled binary re-reads whatever `bunfig.toml` sits in its **CWD**, and
 * `tui/bunfig.toml` names a `preload` its embedded graph cannot resolve — so
 * running it from inside `tui/` dies with `preload not found`, and the failure
 * reads as a broken release build. Every pane below therefore starts in a
 * scratch directory, the same rule `scripts/smoke.sh` follows.
 */

import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CWD = new URL("../", import.meta.url).pathname;
const FIXTURES = join(CWD, "fixtures");

/**
 * `tmux` by absolute path.
 *
 * A shell may define `tmux` as a function (oh-my-zsh's plugin does), which a
 * non-interactive `Bun.spawnSync` cannot see — it fails with
 * `_zsh_tmux_plugin_run: command not found` and looks like tmux is missing.
 */
const TMUX = "/usr/bin/tmux";

/** How a pane gets its data. */
type Transport =
  | { kind: "fixture"; fixture: string }
  | { kind: "live"; socket: string };

/** A driven tmux session running the compiled binary. */
class Pane {
  private readonly session: string;
  private readonly stateDir: string;
  /** The pane's CWD — deliberately *not* `tui/`; see the module doc. */
  private readonly scratch: string;

  constructor(
    binary: string,
    transport: Transport,
    cols: number,
    rows: number,
  ) {
    this.session = `buzz-m2-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
    this.stateDir = mkdtempSync(join(tmpdir(), "buzz-m2-state-"));
    this.scratch = mkdtempSync(join(tmpdir(), "buzz-m2-cwd-"));

    const env =
      transport.kind === "fixture"
        ? `BUZZ_TUI_FIXTURE=${join(FIXTURES, `${transport.fixture}.jsonl`)} ` +
          "BUZZ_TUI_FIXED_TIME=2026-08-04T14:12:00Z "
        : `BUZZ_DAEMON_SOCKET=${transport.socket} `;

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
      // The shell outlives the binary on purpose. `remain-on-exit on` *clears*
      // a dead pane and prints "Pane is dead", so a crash's message — the one
      // thing worth capturing — would be gone by the time anything read it.
      `XDG_STATE_HOME=${this.stateDir} ${env}` +
        `BUZZ_TUI_NO_ANIM=1 TZ=UTC LANG=C.UTF-8 ${binary}; sleep 600`,
    ]);

    // `-x/-y` at creation is silently ignored on a box already running tmux.
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
  /** `null` means "attach to the live daemon". */
  readonly fixture: string | null;
  readonly steps: readonly Step[];
}

/**
 * The live walk — every layer reachable against a daemon with no relay ingest.
 *
 * Deliberately includes the two empty lists. An empty list that *names its own
 * cause* is the §1.3-property-3 fix landing, and it is only observable here:
 * every fixture is populated, so this is the one configuration in which that
 * code path renders at all.
 */
const LIVE: readonly Scenario[] = [
  {
    id: "L01-home-live",
    title: "L0 HOME — live daemon, real identity",
    fixture: null,
    steps: [
      {
        key: "(boot)",
        expect: "PLACES",
        note: "attached over BUZZ_DAEMON_SOCKET; statusline shows the real relay + pubkey",
      },
    ],
  },
  {
    id: "L02-channels-live",
    title: "L1 CHANNELS — empty, and saying why (§1.3 property 3)",
    fixture: null,
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "home › channels",
        note: "descend to L1; the list names the connection state rather than painting blank rows",
      },
    ],
  },
  {
    id: "L03-agents-live",
    title: "L1 AGENTS — the same empty-state contract on a second list",
    fixture: null,
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      { key: "↓", send: "Down", expect: "❯ Agents", note: "step to Agents" },
      {
        key: "→",
        send: "Right",
        expect: "home › agents",
        note: "descend; one empty-state module serves both lists",
      },
    ],
  },
  {
    id: "L04-search-live",
    title: "SEARCH — ctrl+k against a live daemon",
    fixture: null,
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
        expect: "no results",
        note: "a real query against an unmounted /search: honest zero, not a hang",
      },
    ],
  },
  {
    id: "L05-wave2-live",
    title: "§4.1.3 — an unshipped destination is tagged, never silent",
    fixture: null,
    steps: [
      { key: "(boot)", expect: "PLACES", note: "home" },
      {
        key: "↓ ×2",
        send: "Down",
        expect: "❯ Agents",
        note: "step past Channels",
      },
      { key: "", send: "Down", expect: "❯ Settings", note: "onto Settings" },
      {
        key: "→",
        send: "Right",
        expect: "Wave 2",
        note: "descent refused — but the row carries the tag, so the refusal is legible",
      },
    ],
  },
];

/**
 * The deep walk — L2/L3/L4 through the **compiled** binary on fixture data.
 *
 * Same keystrokes as NAVIGATION.md §4. These layers have no live counterpart
 * until the relay ingest lands, and that absence is the milestone's headline
 * rather than something to paper over.
 */
const FIXTURE: readonly Scenario[] = [
  {
    id: "F01-home",
    title: "L0 HOME — attention groups (compiled binary, fixture transport)",
    fixture: "seeded-basic",
    steps: [
      {
        key: "(boot)",
        expect: "ATTENTION",
        note: "§1.1 default selection lands on the top mention",
      },
    ],
  },
  {
    id: "F02-timeline",
    title: "L2 TIMELINE — §4.4's two-keystroke teleport",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      {
        key: "→",
        send: "Right",
        expect: "#engineering",
        note: "teleport straight to L2 at the mention, skipping L1",
      },
    ],
  },
  {
    id: "F03-thread",
    title: "L3 THREAD — §4.2's ⇧↑ thread-leap then →",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
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
        note: "leap to the previous thread root",
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
    id: "F04-drawer",
    title: "DRAWER PEEK — §4.3's ↓ from an empty composer",
    fixture: "agent-stream",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      {
        key: "↓",
        send: "Down",
        expect: "↑/↓ select",
        note: "§2.4 drawer replaces the statusline; chat keeps streaming",
      },
    ],
  },
  {
    id: "F05-activity",
    title: "L4 ACTIVITY — → from the expanded drawer commits to the transcript",
    fixture: "agent-stream",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      { key: "↓", send: "Down", expect: "↑/↓ select", note: "drawer peek" },
      {
        // The expansion's own footer (`drawer.ts:332`), which the collapsed
        // list does not carry — so this marker cannot match the frame before
        // the key, which is the failure mode a looser marker produces.
        key: "⏎",
        send: "Enter",
        expect: "→ open full activity",
        note: "§2.4 expand in place — chat compressed, never replaced [G16]",
      },
      {
        // `steer <agent>`, not "activity": the word "activity" is already on
        // screen in the *drawer's* expansion header, so waiting for it would
        // match the frame before the key and capture L2 while claiming L4.
        // The composer placeholder is the layer's own signature (§2.2).
        key: "→",
        send: "Right",
        expect: "steer ",
        note: "commit to L4 — the composer retargets to steer the agent; ← returns to #engineering [G15]",
      },
    ],
  },
  {
    id: "F06-mention-picker",
    title: "MENTION PICKER — @ over the resolved directory ([D-2])",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      {
        key: "type '@'",
        type: "@",
        expect: "⏎/⇥ insert",
        note: "§3.3 picker opens; agents and people interleaved by rank",
      },
    ],
  },
  {
    id: "F07-search",
    title: "SEARCH — hits render as the query is typed",
    fixture: "seeded-basic",
    steps: [
      { key: "(boot)", expect: "ATTENTION", note: "home" },
      { key: "→", send: "Right", expect: "#engineering", note: "teleport" },
      { key: "ctrl+k", send: "C-k", expect: "› search", note: "§3.5 opens" },
      {
        key: "type 'read'",
        type: "read",
        expect: "read-state slots cap",
        note: "incremental hits",
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
  if (!outdir || !binary) {
    console.error(
      "usage: bun run scripts/dogfood-capture-m2.ts <outdir> <binary> [daemon-socket]",
    );
    process.exit(1);
  }

  const scenarios = socket ? [...LIVE, ...FIXTURE] : FIXTURE;
  const transcript: string[] = [
    "# Buzz TUI — dogfood walkthrough (M2)",
    "",
    "Every frame below came from a real tmux PTY driving the **compiled**",
    `binary (\`${binary.split("/").pop()}\`), not \`bun run src/main.ts\` and not a`,
    "render unit test. Each block names the keystrokes that produced it.",
    "",
    "Two transports, and the split is the milestone's headline:",
    "",
    "- **`live`** — attached to a running `buzz-daemon` over `BUZZ_DAEMON_SOCKET`,",
    "  holding the real claude-test identity against the real relay. Read-only:",
    "  nothing in this walk posts, reacts, or publishes.",
    "- **`fixture`** — the same compiled binary on `BUZZ_TUI_FIXTURE`, because the",
    "  daemon's relay ingest is not built yet, so L2/L3/L4 have nothing to render",
    "  against a live daemon. That gap is the finding, not a capture artifact.",
    "",
  ];

  for (const scenario of scenarios) {
    const transport: Transport = scenario.fixture
      ? { kind: "fixture", fixture: scenario.fixture }
      : { kind: "live", socket: socket as string };

    transcript.push(
      `## ${scenario.title}`,
      "",
      `Transport: \`${transport.kind === "live" ? "live daemon" : scenario.fixture}\``,
      "",
    );

    for (const width of WIDTHS) {
      const dir = join(outdir, `captures-${width.label}`);
      mkdirSync(dir, { recursive: true });

      const pane = new Pane(binary, transport, width.cols, width.rows);
      try {
        const keylog: string[] = [];
        for (const step of scenario.steps) {
          if (step.send) pane.send(step.send);
          if (step.type) pane.type(step.type);
          await pane.waitFor(step.expect);
          keylog.push(`${step.key.padEnd(12)} → ${step.note}`);
        }
        // One settle pass, so a frame caught mid-redraw is never what is
        // written. The wait above proves the marker arrived; it does not prove
        // the rest of the frame has finished painting.
        await Bun.sleep(400);
        const frame = pane.capture();

        const header =
          `${scenario.title}\n` +
          `geometry:  ${width.cols}x${width.rows}\n` +
          `transport: ${transport.kind === "live" ? `live daemon (${socket})` : `fixture ${scenario.fixture}`}\n` +
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
