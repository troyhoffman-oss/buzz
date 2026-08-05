/**
 * The onboarding wizard's shell — dimensions, keyboard, and the one effect.
 *
 * As thin as `shell/Shell.tsx`, and for the same reason: the screen is computed
 * as `string[]` by `onboarding/screen.ts` and the state transitions live in
 * `onboarding/flow.ts`, so everything a test would want to drive is reachable
 * without a terminal. What is left here is the terminal itself and the one
 * place a secret leaves the process.
 */

import { useKeyboard, useTerminalDimensions } from "@opentui/solid";
import { For, createMemo, createSignal } from "solid-js";
import { resolveTheme } from "../theme/theme";
import { color } from "../theme/tokens";
import {
  type OnboardingState,
  applyOnboardingKey,
  initialOnboarding,
  onboardingFailed,
  onboardingSucceeded,
  takeOnboardingEffect,
} from "./flow";
import type { ProvisionResult } from "./provision";
import { ProvisionFailed, runProvision } from "./provision";
import {
  fitsOnboarding,
  renderOnboarding,
  renderOnboardingFloor,
} from "./screen";

/** Props for {@link Wizard}. */
export interface WizardProps {
  /** Path of the `buzz-daemon` binary that does the protocol work. */
  readonly daemonBinary: string;
  /** Called once provisioning succeeds; the caller then boots the app. */
  readonly onComplete: (
    result: ProvisionResult & { relayUrl: string; communityName: string },
  ) => void;
  /** Called on `ctrl+c`. */
  readonly onQuit: () => void;
  /** Pre-fills, for a re-run after a partial setup. */
  readonly defaults?: { relayUrl?: string; communityName?: string };
}

/** Normalize an OpenTUI key event, mirroring `Shell.tsx`'s `toKeyPress`. */
function toKey(key: {
  name?: string;
  ctrl?: boolean;
  shift?: boolean;
  meta?: boolean;
  sequence?: string;
}): { name: string; ctrl?: boolean; char?: string } {
  const sequence = key.sequence ?? "";
  const first = sequence.codePointAt(0) ?? 0;
  const isControl = first < 0x20 || first === 0x7f;
  const printable =
    !key.ctrl && !key.meta && sequence.length > 0 && !isControl
      ? sequence
      : undefined;
  return {
    name: key.name ?? "",
    ...(key.ctrl ? { ctrl: true } : {}),
    ...(printable !== undefined ? { char: printable } : {}),
  };
}

/** The first-run wizard. */
export function Wizard(props: WizardProps) {
  const dimensions = useTerminalDimensions();
  const [state, setState] = createSignal<OnboardingState>(
    initialOnboarding(props.defaults ?? {}),
  );

  useKeyboard((raw) => {
    const key = toKey(raw);

    // `ctrl+c` quits outright. Unlike the main shell there is no composer to
    // clear first: everything on screen is either a question or a secret, and
    // "clear the field" is what `backspace` is for.
    if (key.ctrl && key.name === "c") {
      props.onQuit();
      return;
    }

    const next = applyOnboardingKey(state(), key);
    const [effect, cleared] = takeOnboardingEffect(next);
    setState(cleared);
    if (effect) void provision(effect);
  });

  /**
   * Perform the provision effect.
   *
   * Failures route back to the field that can fix them
   * ({@link onboardingFailed}) rather than aborting the wizard: an operator who
   * mistyped an unlock passphrase should land on the unlock field with the
   * daemon's message, not at a shell prompt having lost four answers.
   */
  async function provision(
    effect: NonNullable<ReturnType<typeof takeOnboardingEffect>[0]>,
  ): Promise<void> {
    try {
      const result = await runProvision(props.daemonBinary, effect);
      // Secrets are wiped **before** the caller runs, so the passphrase is not
      // live in this component while the daemon spawn is in flight.
      setState((s) => onboardingSucceeded(s));
      props.onComplete({
        ...result,
        relayUrl: effect.relayUrl,
        communityName: effect.communityName,
      });
    } catch (error) {
      const message =
        error instanceof ProvisionFailed
          ? error.message
          : "provisioning failed for an unknown reason";
      setState((s) => onboardingFailed(s, message));
    }
  }

  const lines = createMemo(() => {
    const { width, height } = dimensions();
    return fitsOnboarding(width, height)
      ? renderOnboarding(state(), width, height)
      : renderOnboardingFloor(width, height);
  });

  const theme = resolveTheme();
  const fg = color(theme, "text");
  const bg = color(theme, "background");

  return (
    <box
      style={{
        flexDirection: "column",
        width: "100%",
        height: "100%",
        ...(bg !== undefined ? { backgroundColor: bg } : {}),
      }}
    >
      <For each={lines()}>
        {(line) => <text style={fg !== undefined ? { fg } : {}}>{line}</text>}
      </For>
    </box>
  );
}
