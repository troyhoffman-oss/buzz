/**
 * First-run onboarding — the pure state machine.
 *
 * DESIGN.md §4's owner directive is binding on this:
 *
 * > the TUI must mirror the desktop's full user journey, **from first-run
 * > onboarding (relay, key create/import, community join)** through general-
 * > course agent addition.
 *
 * Mirroring the desktop means mirroring its *steps*, not its widgets. The
 * desktop's `OnboardingFlow` is identity → profile → avatar → backup → download
 * key; the terminal's equivalent drops avatar (an image), keeps the identity
 * and backup halves as one — because [D-8] makes the identity blob *and* the
 * backup the same artifact here — and adds the relay, which the desktop gets
 * from a build-time community config the TUI does not have.
 *
 * So: **relay → identity → passphrase → community label → done.** Four
 * questions, in the order a person can answer them.
 *
 * # This module is pure, and that is what makes it testable
 *
 * `(state, key) → state` with an `effect` slot the shell drains, exactly like
 * `app/dispatch.ts`. The scrypt call, the file write, and the spawn all happen
 * outside; the flow only ever decides. That is why every screen and every
 * validation below is a unit test rather than a PTY run — the PTY run then
 * exists to prove the *keys* arrive, which is what a PTY is actually for.
 *
 * # Auth discipline, restated as three properties this file holds
 *
 * 1. **The passphrase never reaches argv or the environment.** It lives in
 *    {@link OnboardingState.secret} and leaves only through the
 *    {@link ProvisionEffect} the shell hands to `buzz-daemon` **on stdin**.
 * 2. **Nothing secret is rendered back.** {@link renderOnboarding} masks the
 *    passphrase and the imported key; §2.5's redaction discipline is about
 *    logs, and a terminal frame is a log with a scrollback buffer.
 * 3. **No key material is ever parsed here.** The imported string is carried
 *    verbatim to the daemon, which decides whether it is an `nsec`, a hex
 *    secret, or an encrypted backup. §6.4's gate makes that mechanical: this
 *    file cannot even name the encodings.
 */

/** Where the wizard is. */
export type OnboardingStep =
  /** The one screen that is not a question — what this is about to do. */
  | "welcome"
  /** Relay websocket URL. */
  | "relay"
  /** Create a new identity, or import an existing one. */
  | "identityChoice"
  /** Paste an existing key (import only). */
  | "identityImport"
  /** Passphrase that opens an imported encrypted backup (import only). */
  | "importUnlock"
  /** Passphrase the identity is stored under. */
  | "passphrase"
  /** Passphrase again — a typo here loses the account. */
  | "passphraseConfirm"
  /** Community label for the statusline. */
  | "community"
  /** Working: the daemon is running scrypt. */
  | "provisioning"
  /** Provisioned; the shell takes over. */
  | "done";

/** Which identity path the operator chose. */
export type IdentityChoice = "create" | "import";

/**
 * Minimum passphrase length.
 *
 * Must equal `provision::MIN_PASSPHRASE_LEN` in the daemon, and
 * `test/unit/onboarding.test.ts` asserts it against that source rather than
 * restating the number — two floors that drift produce a wizard that accepts a
 * passphrase the daemon then rejects, after the operator has typed it twice.
 */
export const MIN_PASSPHRASE_LEN = 12;

/** The effect the shell performs when the flow is ready to provision. */
export interface ProvisionEffect {
  readonly kind: "provision";
  readonly choice: IdentityChoice;
  /** Key material for an import, verbatim and unparsed. Empty for a create. */
  readonly secret: string;
  /** Passphrase the blob is written under. */
  readonly passphrase: string;
  /** Passphrase that opens an imported encrypted backup, when one was given. */
  readonly unlock: string;
  readonly relayUrl: string;
  readonly communityName: string;
}

/** Everything the wizard holds. */
export interface OnboardingState {
  readonly step: OnboardingStep;
  /** The field being edited. */
  readonly input: string;
  readonly cursor: number;
  readonly relayUrl: string;
  readonly choice: IdentityChoice;
  /** Selection index on {@link OnboardingStep} `identityChoice`. */
  readonly choiceIndex: number;
  readonly communityName: string;
  /**
   * The secrets, held only until the provision effect is drained.
   *
   * Grouped in one object so {@link clearSecrets} is one call rather than three
   * fields somebody can forget — the failure of forgetting is a passphrase
   * sitting in a live object for the rest of the session.
   */
  readonly secret: {
    readonly imported: string;
    readonly passphrase: string;
    readonly unlock: string;
  };
  /** A validation or provisioning failure, rendered under the field. */
  readonly error: string | null;
  /** Set when the flow wants the shell to provision. */
  readonly pending: ProvisionEffect | null;
}

/** The initial state, optionally pre-filled from the environment. */
export function initialOnboarding(
  defaults: { relayUrl?: string; communityName?: string } = {},
): OnboardingState {
  return {
    step: "welcome",
    input: "",
    cursor: 0,
    relayUrl: defaults.relayUrl ?? "",
    choice: "create",
    choiceIndex: 0,
    communityName: defaults.communityName ?? "",
    secret: { imported: "", passphrase: "", unlock: "" },
    error: null,
    pending: null,
  };
}

/** A keypress, in the same shape `nav/keys.ts` reads. */
export interface OnboardingKey {
  readonly name: string;
  readonly shift?: boolean;
  readonly ctrl?: boolean;
  readonly char?: string;
}

/**
 * Whether a step's field is a secret.
 *
 * Drives both the mask in {@link renderOnboarding} and — more importantly —
 * where the typed characters are stored. A secret step writes to
 * {@link OnboardingState.secret}; a plain step writes to `input`. Keeping that
 * decision in one predicate is what stops a new secret step from being added
 * with its value in the visible field.
 */
export function isSecretStep(step: OnboardingStep): boolean {
  return (
    step === "identityImport" ||
    step === "importUnlock" ||
    step === "passphrase" ||
    step === "passphraseConfirm"
  );
}

/**
 * Validate a relay URL without parsing the protocol.
 *
 * A scheme check, not a reachability check. Reachability is the daemon's to
 * discover and report through `connection.state` (§2.6) — probing it here would
 * mean the wizard blocking on a network round trip, and an operator onboarding
 * on a plane would be unable to finish setup for a relay that will be fine
 * tomorrow.
 */
export function validateRelay(url: string): string | null {
  const trimmed = url.trim();
  if (trimmed.length === 0) return "a relay URL is required";
  if (!trimmed.startsWith("ws://") && !trimmed.startsWith("wss://")) {
    return "relay URL must start with wss:// (or ws:// for a local relay)";
  }
  if (trimmed.length <= "wss://".length) return "relay URL has no host";
  return null;
}

/** Validate the storage passphrase. Mirrors the daemon's own floor. */
export function validatePassphrase(passphrase: string): string | null {
  if (passphrase.length < MIN_PASSPHRASE_LEN) {
    return `passphrase must be at least ${MIN_PASSPHRASE_LEN} characters`;
  }
  return null;
}

/** Wipe the secret fields once they are no longer needed. */
function clearSecrets(state: OnboardingState): OnboardingState {
  return { ...state, secret: { imported: "", passphrase: "", unlock: "" } };
}

/** Store a step's edited value into whichever field that step owns. */
function commitInput(
  state: OnboardingState,
  step: OnboardingStep,
  value: string,
): OnboardingState {
  switch (step) {
    case "relay":
      return { ...state, relayUrl: value.trim() };
    case "community":
      return { ...state, communityName: value.trim() };
    case "identityImport":
      return { ...state, secret: { ...state.secret, imported: value.trim() } };
    case "importUnlock":
      return { ...state, secret: { ...state.secret, unlock: value } };
    case "passphrase":
      return { ...state, secret: { ...state.secret, passphrase: value } };
    default:
      return state;
  }
}

/** Move to `step` with an empty field. */
function goTo(state: OnboardingState, step: OnboardingStep): OnboardingState {
  return { ...state, step, input: "", cursor: 0, error: null };
}

/** Reject with a message, keeping what was typed. */
function reject(state: OnboardingState, error: string): OnboardingState {
  return { ...state, error };
}

/**
 * The step reached by `⏎` from `step`, given the current state.
 *
 * Separated from {@link applyOnboardingKey} so the branch table is readable and
 * so a test can assert the whole graph without synthesizing keypresses. The
 * import-only steps are skipped for a create, which is the one place the flow
 * is not linear.
 */
function advance(state: OnboardingState): OnboardingState {
  const value = state.input;
  switch (state.step) {
    case "welcome":
      return goTo(state, "relay");

    case "relay": {
      const error = validateRelay(value);
      if (error) return reject(state, error);
      return goTo(commitInput(state, "relay", value), "identityChoice");
    }

    case "identityChoice": {
      const choice: IdentityChoice =
        state.choiceIndex === 0 ? "create" : "import";
      const next = { ...state, choice };
      return goTo(next, choice === "create" ? "passphrase" : "identityImport");
    }

    case "identityImport": {
      if (value.trim().length === 0) {
        return reject(
          state,
          "paste a key, or press esc to go back and create one",
        );
      }
      // Whether this is an encrypted backup — and therefore whether an unlock
      // passphrase is needed — is the daemon's call, not ours: deciding it here
      // would mean this file recognising key encodings, which §6.4 forbids. So
      // the unlock step is always offered and always skippable.
      return goTo(commitInput(state, "identityImport", value), "importUnlock");
    }

    case "importUnlock":
      // Empty is legitimate: an `nsec` has no unlock passphrase. The daemon
      // says so if it needed one, and the flow returns here with that message.
      return goTo(commitInput(state, "importUnlock", value), "passphrase");

    case "passphrase": {
      const error = validatePassphrase(value);
      if (error) return reject(state, error);
      return goTo(commitInput(state, "passphrase", value), "passphraseConfirm");
    }

    case "passphraseConfirm": {
      if (value !== state.secret.passphrase) {
        // Back to the first entry rather than letting them retype the
        // confirmation: if the two disagree, the *first* one is as likely to be
        // the typo, and confirming a typo twice is how an account is lost.
        return {
          ...goTo(state, "passphrase"),
          secret: { ...state.secret, passphrase: "" },
          error: "the two passphrases do not match — enter it again",
        };
      }
      return goTo(state, "community");
    }

    case "community": {
      const name = value.trim();
      if (name.length === 0)
        return reject(state, "a community name is required");
      const next = commitInput(state, "community", value);
      return {
        ...next,
        step: "provisioning",
        input: "",
        cursor: 0,
        error: null,
        pending: {
          kind: "provision",
          choice: next.choice,
          secret: next.secret.imported,
          passphrase: next.secret.passphrase,
          unlock: next.secret.unlock,
          relayUrl: next.relayUrl,
          communityName: name,
        },
      };
    }

    default:
      return state;
  }
}

/** The step `esc` returns to, or `null` when there is nowhere back. */
export function previousStep(state: OnboardingState): OnboardingStep | null {
  switch (state.step) {
    case "relay":
      return "welcome";
    case "identityChoice":
      return "relay";
    case "identityImport":
      return "identityChoice";
    case "importUnlock":
      return "identityImport";
    case "passphrase":
      return state.choice === "import" ? "importUnlock" : "identityChoice";
    case "passphraseConfirm":
      return "passphrase";
    case "community":
      return "passphraseConfirm";
    default:
      // `welcome` has nowhere back and `provisioning` must not be interrupted:
      // the daemon is mid-scrypt and a cancel would leave a half-written
      // identity directory the next run would refuse as "already provisioned".
      return null;
  }
}

/** Apply one keypress. Pure. */
export function applyOnboardingKey(
  state: OnboardingState,
  key: OnboardingKey,
): OnboardingState {
  if (state.step === "provisioning" || state.step === "done") return state;

  if (key.name === "return" || key.name === "enter") return advance(state);

  if (key.name === "escape") {
    const back = previousStep(state);
    if (!back) return state;
    // Stepping back off a secret field clears it. Keeping it would mean a
    // passphrase surviving in memory for a step the operator explicitly left,
    // and would silently pre-fill a field that renders as empty.
    const cleared = isSecretStep(state.step)
      ? commitInput(state, state.step, "")
      : state;
    return goTo(cleared, back);
  }

  if (state.step === "identityChoice") {
    if (key.name === "up") {
      return { ...state, choiceIndex: Math.max(0, state.choiceIndex - 1) };
    }
    if (key.name === "down") {
      return { ...state, choiceIndex: Math.min(1, state.choiceIndex + 1) };
    }
    if (key.name === "right") return advance(state);
    return state;
  }

  if (key.name === "backspace") {
    if (state.cursor === 0) return state;
    const text =
      state.input.slice(0, state.cursor - 1) + state.input.slice(state.cursor);
    return { ...state, input: text, cursor: state.cursor - 1, error: null };
  }

  if (key.name === "left") {
    return { ...state, cursor: Math.max(0, state.cursor - 1) };
  }
  if (key.name === "right") {
    return { ...state, cursor: Math.min(state.input.length, state.cursor + 1) };
  }

  if (key.char !== undefined && !key.ctrl) {
    const text =
      state.input.slice(0, state.cursor) +
      key.char +
      state.input.slice(state.cursor);
    return {
      ...state,
      input: text,
      cursor: state.cursor + key.char.length,
      error: null,
    };
  }

  return state;
}

/** Drain the provision effect, returning it and the cleared state. */
export function takeOnboardingEffect(
  state: OnboardingState,
): [ProvisionEffect | null, OnboardingState] {
  if (!state.pending) return [null, state];
  return [state.pending, { ...state, pending: null }];
}

/**
 * Report a provisioning failure and return to the field that can fix it.
 *
 * The routing matters more than the message. A daemon that says "its own
 * passphrase is needed to open it" is telling the operator to fill in the
 * unlock field — dropping them back on the relay screen with that text would be
 * a message with no action attached, which §1.3 property 2 forbids.
 */
export function onboardingFailed(
  state: OnboardingState,
  message: string,
): OnboardingState {
  const lower = message.toLowerCase();
  if (
    lower.includes("passphrase is needed") ||
    lower.includes("wrong passphrase")
  ) {
    return { ...goTo(state, "importUnlock"), error: message };
  }
  if (lower.includes("unrecognized key") || lower.includes("invalid")) {
    return { ...goTo(state, "identityImport"), error: message };
  }
  if (lower.includes("already provisioned")) {
    return { ...goTo(state, "identityChoice"), error: message };
  }
  // Anything else is not attributable to one field, so it lands on the last
  // question with the daemon's own words rather than being reshaped into a
  // guess about which step was wrong.
  return { ...goTo(state, "community"), error: message };
}

/** Mark the flow finished and wipe every secret it held. */
export function onboardingSucceeded(state: OnboardingState): OnboardingState {
  return clearSecrets({ ...state, step: "done", input: "", error: null });
}
