/**
 * Dogfood M2 — first-run onboarding capture.
 *
 * Runs the **compiled** `buzz-tui` with **no configuration at all**, in a
 * sandbox with its own `HOME` and XDG roots, and captures the welcome flow at
 * both widths. This is the one path that cannot be exercised any other way: it
 * is defined entirely by the *absence* of state, so a developer box — which has
 * a config by the second run — can never show it again.
 *
 * # What this deliberately does not do
 *
 * It stops at the last question and never presses the final `⏎`. Provisioning
 * writes a key to disk and then spawns a daemon that connects to a relay; the
 * dogfood brief is read-only against the live community, and a wizard run that
 * completed would create a fresh identity and dial out. Every *screen* is
 * reachable before that point, so nothing visual is lost by stopping.
 *
 * The relay the wizard is given is therefore a **non-routable placeholder**,
 * not the live one. If a stray keystroke ever did complete the flow, the spawn
 * fails to connect rather than joining the real community with a scratch key.
 *
 * Usage: bun run scripts/dogfood-onboarding-m2.ts <outdir> <binary>
 */

import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CWD = new URL("../", import.meta.url).pathname;

/** See `dogfood-capture-m2.ts` — a shell may shadow `tmux` with a function. */
const TMUX = "/usr/bin/tmux";

/**
 * A relay URL that cannot resolve.
 *
 * `.invalid` is reserved by RFC 2606 precisely so it can never be delegated, so
 * this is guaranteed inert rather than merely unlikely to belong to anyone.
 */
const PLACEHOLDER_RELAY = "wss://relay.dogfood-m2.invalid";

/**
 * The passphrase typed into the scratch wizard.
 *
 * Not a secret by any definition — it protects nothing, because the flow is
 * never completed and no key is ever minted. Named as a constant so the
 * confirm step retypes *the same* string rather than a second literal that
 * could drift and silently exercise the mismatch path instead.
 */
const SCRATCH_PASSPHRASE = "dogfood-m2-scratch-passphrase";

class Pane {
  private readonly session: string;
  /** A sandbox that stands in for a machine that has never seen Buzz. */
  readonly home: string;

  constructor(
    binary: string,
    daemonBinary: string,
    cols: number,
    rows: number,
  ) {
    this.session = `buzz-m2-onb-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
    this.home = mkdtempSync(join(tmpdir(), "buzz-m2-home-"));
    const scratch = mkdtempSync(join(tmpdir(), "buzz-m2-onb-cwd-"));

    this.tmux([
      "new-session",
      "-d",
      "-s",
      this.session,
      "-c",
      scratch,
      "-x",
      String(cols),
      "-y",
      String(rows),
      // Every XDG root is redirected, not just HOME: the startup path resolves
      // config through `XDG_CONFIG_HOME` and falls back to `$HOME/.config`, so
      // setting only one of the two leaves a real developer config reachable —
      // and the wizard would never appear, which is the whole subject here.
      `HOME=${this.home} XDG_CONFIG_HOME=${this.home}/.config ` +
        `XDG_DATA_HOME=${this.home}/.local/share ` +
        `XDG_STATE_HOME=${this.home}/.local/state ` +
        `XDG_RUNTIME_DIR=${this.home}/run ` +
        // `main.ts` resolves the daemon binary *before* deciding to onboard and
        // exits 1 when it cannot find one — correctly, since the wizard's last
        // act is to spawn it. A sandbox HOME has no `PATH` entry for it, so it
        // is named explicitly. The wizard is still never completed, so this
        // binary is located and never run.
        `BUZZ_DAEMON_BIN=${daemonBinary} ` +
        `BUZZ_TUI_NO_ANIM=1 TZ=UTC LANG=C.UTF-8 ${binary}; sleep 600`,
    ]);
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

interface Step {
  readonly id: string;
  readonly title: string;
  readonly key: string;
  readonly send?: string;
  readonly type?: string;
  readonly expect: string;
  readonly note: string;
}

/**
 * The wizard, in order — DESIGN.md §4's "relay → identity → passphrase →
 * community label → done".
 *
 * Captured as a *sequence in one pane* rather than as independent runs: the
 * screens are only reachable through each other, and a per-screen pane would
 * have to re-answer every earlier question anyway.
 */
const STEPS: readonly Step[] = [
  {
    id: "O1-welcome",
    title: "WELCOME — the one screen that is not a question",
    key: "(boot)",
    // The title, not the word "buzz": "buzz" also appears in the
    // `cannot find buzz-daemon` failure, so it would match a dead pane and
    // capture an error while claiming a welcome screen.
    expect: "Welcome to Buzz",
    note: "plain `buzz-tui` on a machine with no config: a welcome flow, not exit 1",
  },
  {
    id: "O2-relay",
    title: "RELAY — the question the desktop never has to ask",
    key: "\u23ce",
    send: "Enter",
    expect: "Which relay?",
    note: "the desktop gets its relay from a build-time community config; the TUI must ask",
  },
  {
    id: "O3-identity-choice",
    title: "IDENTITY — create or import ([D-8])",
    key: "type url, \u23ce",
    type: `${PLACEHOLDER_RELAY}\r`,
    expect: "Create an identity, or bring one?",
    note: "a non-routable placeholder relay — this capture must not join the real community",
  },
  {
    id: "O4-passphrase",
    title: "PASSPHRASE — the identity's protection at rest (NIP-49)",
    key: "\u23ce (create)",
    send: "Enter",
    expect: "Choose a passphrase",
    note: "create is the default; import takes the same path after one extra screen",
  },
  {
    id: "O5-passphrase-masked",
    title: "PASSPHRASE — typed, and masked in the frame",
    key: "type 12+ chars",
    type: SCRATCH_PASSPHRASE,
    expect: "\u2022",
    note: "\u00a72.5: nothing secret is rendered back — a terminal frame is a log with a scrollback buffer",
  },
  {
    id: "O6-passphrase-confirm",
    title: "CONFIRM — a typo here would lock you out, so it is asked twice",
    key: "\u23ce",
    send: "Enter",
    expect: "Type it again",
    note: "the second ask is the only defence: there is no reset for a key",
  },
  {
    id: "O7-community",
    title: "COMMUNITY — a label for the statusline, not a join",
    key: "retype, \u23ce",
    type: `${SCRATCH_PASSPHRASE}\r`,
    expect: "What is this community called?",
    note: "membership is a relay-side action with no Wave-1 endpoint; the copy says label, not join",
  },
];

const WIDTHS = [
  { cols: 120, rows: 36, label: "120col" },
  { cols: 60, rows: 30, label: "60col" },
] as const;

async function main(): Promise<void> {
  const [outdir, binary, daemonBinary] = process.argv.slice(2);
  if (!outdir || !binary || !daemonBinary) {
    console.error(
      "usage: bun run scripts/dogfood-onboarding-m2.ts <outdir> <binary> <daemon-binary>",
    );
    process.exit(1);
  }
  mkdirSync(outdir, { recursive: true });

  const transcript: string[] = [
    "# Buzz TUI — first-run onboarding (M2)",
    "",
    "The **compiled** binary, run with no configuration in a sandbox with its own",
    "`HOME` and XDG roots — a machine that has never seen Buzz. This flow is",
    "defined by the absence of state, so it is unreproducible on any box that has",
    "run `buzz-tui` once.",
    "",
    "**Nothing is provisioned.** The walk stops at the last question and never",
    "presses the final `⏎`, and the relay it is given is a non-routable",
    `\`.invalid\` placeholder (${PLACEHOLDER_RELAY}) rather than the live one — so`,
    "even a stray keystroke could not join the real community with a scratch key.",
    "",
  ];

  for (const width of WIDTHS) {
    const dir = join(outdir, `captures-${width.label}`);
    mkdirSync(dir, { recursive: true });

    const pane = new Pane(binary, daemonBinary, width.cols, width.rows);
    try {
      const keylog: string[] = [];
      for (const step of STEPS) {
        if (step.send) pane.send(step.send);
        if (step.type) pane.type(step.type);
        await pane.waitFor(step.expect);
        await Bun.sleep(300);
        const frame = pane.capture();
        keylog.push(`${step.key.padEnd(16)} → ${step.note}`);

        const header =
          `${step.title}\n` +
          `geometry:  ${width.cols}x${width.rows}\n` +
          `transport: none — first run, no config on disk\n` +
          `binary:    ${binary}\n` +
          `keys so far:\n${keylog.map((k) => `  ${k}`).join("\n")}\n` +
          `${"─".repeat(Math.min(width.cols, 100))}\n`;
        const file = join(dir, `${step.id}.txt`);
        writeFileSync(file, `${header}${frame}`);
        console.log(`wrote ${file}`);

        if (width.cols === 120) {
          transcript.push(
            `## ${step.title}`,
            "",
            `Key: \`${step.key}\` — ${step.note}`,
            "",
            "```",
            frame.replace(/\s+$/, ""),
            "```",
            "",
          );
        }
      }
    } finally {
      pane.kill();
    }
  }

  writeFileSync(join(outdir, "WALKTHROUGH.md"), transcript.join("\n"));
  console.log(`wrote ${join(outdir, "WALKTHROUGH.md")}`);
}

await main();
