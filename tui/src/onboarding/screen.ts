/**
 * The onboarding screen — `OnboardingState` → `string[]`.
 *
 * Same contract as `app/screen.ts`: one pure function returning the exact rows
 * the terminal receives, so a snapshot test compares what the operator sees
 * rather than a tree that might lay out differently.
 *
 * # It is the same shell, one question at a time
 *
 * NAVIGATION.md §2's band stack is not suspended for onboarding. The top rule
 * carries a breadcrumb (`setup › identity`), the composer row is the field, and
 * `❯` marks it — [G8] holds here exactly as it does everywhere else, which is
 * what makes the wizard feel like the app rather than like an installer that
 * happens to precede it. The statusline is replaced by hints, because there is
 * no relay, no identity, and no scope to put in it yet, and rendering an empty
 * one would be three rows of nothing at the moment of first impression.
 *
 * # Nothing secret is rendered back
 *
 * §2.5's redaction discipline applies to the frame: a terminal is a log with a
 * scrollback buffer, and `capture-pane` is a `grep` over it. Secret fields
 * render as `•` per grapheme — a count, not the content — because an operator
 * needs to see that a keystroke landed.
 */

import { FOCUS_GLYPH, rule, topRule } from "../render/bands";
import { graphemes, pad, truncate, wrapHints, wrapText } from "../render/width";
import {
  type OnboardingState,
  type OnboardingStep,
  isSecretStep,
} from "./flow";

/** The mask glyph. One per grapheme, so length is visible and content is not. */
const MASK = "•";

/** The prompt for each step, and the hint under it. */
interface Prompt {
  /** Breadcrumb segment. */
  readonly crumb: string;
  /** The question. */
  readonly title: string;
  /** One or two lines of context under the question. */
  readonly body: readonly string[];
  /**
   * Placeholder shown in an empty field.
   *
   * `""` on a **fieldless** step ({@link isFieldless}), where no field renders
   * and a placeholder would be copy that reads as live and never appears. A
   * PTY run caught exactly that: the welcome screen carried "press enter to
   * begin", which was invisible on screen and therefore unassertable — the
   * affordance is in {@link Prompt.hints}, where it does render.
   */
  readonly placeholder: string;
  /** Key hints for the bottom band. */
  readonly hints: readonly string[];
}

/**
 * The copy, in one table.
 *
 * A table rather than a switch inside the renderer because the copy is the
 * product here: onboarding is the one screen where a wrong word costs a user,
 * and a reviewer should be able to read all of it without reading any layout.
 */
const PROMPTS: Record<OnboardingStep, Prompt> = {
  welcome: {
    crumb: "welcome",
    title: "Welcome to Buzz",
    body: [
      "This is a chat client for the box your agents run on.",
      "",
      "Four questions and you are in: which relay, your identity,",
      "a passphrase to protect it, and what to call this community.",
      "",
      "Your key is generated (or imported) locally and stored",
      "encrypted on this machine. It is never sent anywhere.",
    ],
    placeholder: "",
    hints: ["⏎ begin", "ctrl+c quit"],
  },
  relay: {
    crumb: "setup › relay",
    title: "Which relay?",
    body: [
      "The websocket URL of the Buzz community you are joining.",
      "Your admin has it; it looks like wss://buzz.example.com.",
    ],
    placeholder: "wss://",
    hints: ["⏎ next", "esc back", "ctrl+c quit"],
  },
  identityChoice: {
    crumb: "setup › identity",
    title: "Create an identity, or bring one?",
    body: [
      "Your identity is a keypair. Everything you post is signed with it,",
      "and it is how the relay knows you.",
    ],
    placeholder: "",
    hints: ["↑↓ choose", "⏎ select", "esc back"],
  },
  // The copy here describes what to paste **functionally** — "your secret key",
  // "an encrypted backup" — and deliberately never lists the encodings.
  //
  // Two reasons, and the second is the real one. §6.4's boundary gate bans the
  // spelling outright, which reads at first like the copy fighting the tooling.
  // But the gate is pointing at something true: which encodings are accepted is
  // the daemon's `provision::decode_secret`, and a screen that enumerates them
  // is a second copy of that list which goes stale the day a fourth is added —
  // silently, in the direction of telling an operator their valid key is
  // invalid. The daemon already names the accepted forms in its own error, and
  // `onboardingFailed` routes that error back onto this field, so the
  // authoritative list reaches the operator exactly when it is useful.
  identityImport: {
    crumb: "setup › identity › import",
    title: "Paste your key",
    body: [
      "Your existing secret key, or the contents of an encrypted",
      "backup file — including one exported from the Buzz desktop app.",
      "",
      "It is not shown as you type, and it is not written anywhere",
      "except into the encrypted blob at the end of this flow.",
    ],
    placeholder: "paste and press enter",
    hints: ["⏎ next", "esc back"],
  },
  importUnlock: {
    crumb: "setup › identity › unlock",
    title: "Does that key have a passphrase?",
    body: [
      "An encrypted backup needs the passphrase it was saved with.",
      "A plain secret key does not — leave this empty and continue.",
    ],
    placeholder: "leave empty if none",
    hints: ["⏎ next", "esc back"],
  },
  passphrase: {
    crumb: "setup › passphrase",
    title: "Choose a passphrase",
    body: [
      "This encrypts your key on this machine. You will type it each",
      "time you start Buzz on a cold boot.",
      "",
      "Write it down somewhere safe. There is no reset: the passphrase",
      "is the only thing that opens the key, by design.",
    ],
    placeholder: "at least 12 characters",
    hints: ["⏎ next", "esc back"],
  },
  passphraseConfirm: {
    crumb: "setup › passphrase",
    title: "Type it again",
    body: ["A typo here would lock you out of the identity you just made."],
    placeholder: "confirm",
    hints: ["⏎ next", "esc back"],
  },
  community: {
    crumb: "setup › community",
    title: "What is this community called?",
    body: [
      "A label for the status bar, so you can tell two communities apart.",
      "Anything you like.",
    ],
    placeholder: "e.g. buzz-dev",
    hints: ["⏎ finish", "esc back"],
  },
  provisioning: {
    crumb: "setup › finishing",
    title: "Encrypting your key…",
    body: [
      "This takes a second or two on purpose: the encryption is",
      "deliberately slow so that guessing your passphrase is too.",
    ],
    placeholder: "",
    hints: [],
  },
  done: {
    crumb: "setup › done",
    title: "You are in.",
    body: [],
    placeholder: "",
    hints: [],
  },
};

/** The two rows on the identity-choice step. */
const IDENTITY_CHOICES: ReadonlyArray<readonly [string, string]> = [
  ["Create a new identity", "generate a fresh keypair on this machine"],
  ["Import an existing key", "a secret key, or an encrypted backup file"],
];

/** Whether the flow is showing rather than asking (no field on screen). */
function isFieldless(step: OnboardingStep): boolean {
  return (
    step === "welcome" ||
    step === "identityChoice" ||
    step === "provisioning" ||
    step === "done"
  );
}

/** The two-column indent every band in NAVIGATION §2's frames carries. */
const INDENT = "  ";

/**
 * Render the wizard.
 *
 * Returns exactly `rows` lines, every one exactly `cols` wide — the same
 * contract `renderScreen` holds, so the shell can hand either to OpenTUI
 * without knowing which it got.
 */
export function renderOnboarding(
  state: OnboardingState,
  cols: number,
  rows: number,
): string[] {
  const prompt = PROMPTS[state.step];
  const hintRows = wrapHints(prompt.hints, cols);
  const fieldRows = isFieldless(state.step) ? 0 : 1;
  const errorRows = state.error ? 1 : 0;

  // Bottom-up, because the bands are fixed and the body absorbs what is left —
  // §2's own arithmetic. Computing the body first and letting the bands
  // overflow is how a wizard ends up scrolling its own key hints off screen.
  const chromeRows = 1 + fieldRows + 1 + hintRows.length + errorRows;
  const bodyRows = Math.max(0, rows - chromeRows);

  const body: string[] = [];
  body.push("");
  body.push(
    pad(`${INDENT}${truncate(prompt.title, cols - INDENT.length)}`, cols),
  );
  body.push("");
  for (const line of prompt.body) {
    if (line.length === 0) {
      body.push(pad("", cols));
      continue;
    }
    for (const wrapped of wrapText(line, cols - INDENT.length)) {
      body.push(pad(`${INDENT}${wrapped}`, cols));
    }
  }

  if (state.step === "identityChoice") {
    body.push("");
    IDENTITY_CHOICES.forEach(([label, detail], index) => {
      const marker = index === state.choiceIndex ? FOCUS_GLYPH : " ";
      body.push(pad(`${INDENT}${marker} ${truncate(label, cols - 4)}`, cols));
      body.push(pad(`${INDENT}    ${truncate(detail, cols - 6)}`, cols));
    });
  }

  // Clip rather than scroll: every screen here fits in the 40×16 floor by
  // construction, so a body longer than the frame is a copy edit that made one
  // too long — and it should be visible as a missing line in the snapshot
  // rather than hidden behind a scroll offset nobody drives.
  const frame = body.slice(0, bodyRows);
  while (frame.length < bodyRows) frame.push(pad("", cols));

  frame.push(topRule(prompt.crumb, cols));

  if (fieldRows > 0) {
    const shown = isSecretStep(state.step)
      ? MASK.repeat(graphemes(state.input).length)
      : state.input;
    const text = shown.length > 0 ? shown : prompt.placeholder;
    // The focus glyph is on the field whenever there is one, and on nothing
    // when there is not — never two, never zero-with-a-field [G8].
    frame.push(pad(`${FOCUS_GLYPH} ${truncate(text, cols - 2)}`, cols));
  }

  if (state.error) {
    frame.push(
      pad(`${INDENT}${truncate(state.error, cols - INDENT.length)}`, cols),
    );
  }

  frame.push(rule(cols));
  for (const hint of hintRows) frame.push(pad(hint, cols));

  // Height is exact in both directions: a short frame leaves the previous
  // paint's rows on screen, and a tall one pushes the field off the bottom.
  return frame.slice(0, rows);
}

/** Widths under this cannot hold the wizard legibly (§2.3's floor). */
export const ONBOARDING_MIN_COLS = 40;

/** Whether the terminal can render the wizard at all. */
export function fitsOnboarding(cols: number, rows: number): boolean {
  return cols >= ONBOARDING_MIN_COLS && rows >= 8;
}

/** A one-line fallback under the floor, so the app never renders blank. */
export function renderOnboardingFloor(cols: number, rows: number): string[] {
  const message = truncate("buzz-tui setup needs a wider terminal", cols);
  const lines = [pad(message, cols)];
  while (lines.length < rows) lines.push(pad("", cols));
  return lines.slice(0, rows);
}

/** Exported for the T1 suite: the copy is the product, so it is assertable. */
export function promptFor(step: OnboardingStep): Prompt {
  return PROMPTS[step];
}
