/**
 * The one root component — onboarding, then the app.
 *
 * # Why this file exists at all
 *
 * A first draft ran the wizard and the shell as two `render()` calls in
 * sequence. That is wrong in a way only a real PTY shows: `render()` **never
 * returns** — it owns the terminal for the process's lifetime — so the second
 * call built a second `CliRenderer` over a terminal the first still held, and
 * OpenTUI threw from its own constructor with a stack the operator would read
 * as a crash.
 *
 * The honest structure is the one the failure points at: onboarding is not a
 * separate program that precedes the app, it is the app's **first screen** when
 * there is no identity yet. So there is one renderer and one component, and the
 * switch between the two views is a signal.
 *
 * That also fixes something subtler for free. The passphrase collected by the
 * wizard is handed straight to the spawn, so a first run types it twice (entry
 * and confirmation) rather than three times — and no prompt is ever written to
 * stdout while the renderer owns the screen, which in the two-call shape put
 * `passphrase for identity …` *inside* the rendered frame.
 */

import { Show, createSignal } from "solid-js";
import type { DaemonClient } from "../client/daemon-client";
import { Wizard } from "../onboarding/Wizard";
import type { TuiConfig } from "../startup/paths";
import { Shell } from "./Shell";

/** What the root needs to boot either view. */
export interface RootProps {
  /**
   * The already-connected client, when startup found or spawned a daemon
   * before rendering. `null` means this machine has not onboarded.
   */
  readonly client: DaemonClient | null;
  /** Path of the `buzz-daemon` binary, for the wizard's provisioning step. */
  readonly daemonBinary: string;
  /**
   * Connect after onboarding: write the config, spawn the daemon with the
   * passphrase just collected, and attach.
   *
   * Passed in rather than done here so this component holds no I/O and
   * `main.ts` keeps the whole §2.3 sequence in one readable place.
   */
  readonly connect: (
    config: TuiConfig,
    passphrase: string,
  ) => Promise<DaemonClient>;
  readonly now: () => number;
  readonly onQuit: () => void;
  /** Surfaced when the post-onboarding connect fails. */
  readonly onFatal: (message: string) => void;
}

/** The application root. */
export function Root(props: RootProps) {
  const [client, setClient] = createSignal<DaemonClient | null>(props.client);

  return (
    <Show
      when={client()}
      fallback={
        <Wizard
          daemonBinary={props.daemonBinary}
          onQuit={props.onQuit}
          onComplete={(result) => {
            void props
              .connect(
                {
                  relayUrl: result.relayUrl,
                  pubkey: result.pubkey,
                  communityName: result.communityName,
                },
                result.passphrase,
              )
              .then(setClient)
              .catch((error: unknown) => {
                // A connect failure after a *successful* provision is not the
                // wizard's to render: the identity exists and the answers are
                // on disk, so the remedy is relaunching, not re-answering.
                props.onFatal(
                  error instanceof Error ? error.message : String(error),
                );
              });
          }}
        />
      }
    >
      {(connected: () => DaemonClient) => (
        <Shell client={connected()} now={props.now} onQuit={props.onQuit} />
      )}
    </Show>
  );
}
