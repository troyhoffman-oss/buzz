/**
 * The navigation shell — NAVIGATION.md §1, §2, §5.
 *
 * The four-region rail/list/main/aux shell this file used to hold is gone; §7
 * supersedes it outright:
 *
 * > **§3.0 The shell** — superseded. The four-region rail/list/main/aux shell is
 * > replaced by the layer spine (§1) plus the four bottom bands (§2). **Nothing
 * > is ever side by side.**
 *
 * What remains here is deliberately thin. The screen is computed as `string[]`
 * by `app/screen.ts` and handed to OpenTUI as text; this component owns only
 * the terminal's dimensions, the keyboard, and the daemon subscription. That
 * split is what makes the whole navigation model testable without a terminal —
 * the T1 matrix and §4's walkthrough tests call `renderScreen` directly, and
 * this file has nothing left in it that a test would need to drive.
 */

import { useKeyboard, useTerminalDimensions } from "@opentui/solid";
import { For, createMemo, createSignal, onCleanup, onMount } from "solid-js";
import { applyIntent, withDefaultSelection } from "../app/dispatch";
import { renderScreen, rowContext } from "../app/screen";
import { type AppState, initialState } from "../app/state";
import type { DaemonClient } from "../client/daemon-client";
import { type KeyPress, resolveKey } from "../nav/keys";
import { current } from "../nav/layers";

/** Props for {@link Shell}. */
export interface ShellProps {
  /** The daemon — fixture-backed in tests, UDS-backed in production (§5.4). */
  readonly client: DaemonClient;
  /** Called when the user quits. */
  readonly onQuit: () => void;
  /**
   * Clock injection — `BUZZ_TUI_FIXED_TIME` in T1/T2 (§5.3 requirement 1).
   *
   * Every timestamp the UI renders flows through this, so a snapshot suite is
   * deterministic; a `Date.now()` anywhere below would silently break it.
   */
  readonly now?: () => number;
}

/**
 * Normalize an OpenTUI key event into the {@link KeyPress} the §5 chains read.
 *
 * The chains are pure functions over this shape, so this is the one place a
 * terminal's key encoding is interpreted — and §6.4's "no hardcoded key string
 * in a handler" stays true because the key *table* lives in `nav/keys.ts`.
 */
function toKeyPress(key: {
  name?: string;
  shift?: boolean;
  ctrl?: boolean;
  meta?: boolean;
  sequence?: string;
}): KeyPress {
  const name = key.name ?? "";
  const sequence = key.sequence ?? "";
  // A printable is a non-control sequence with no ctrl/meta modifier. Testing
  // the *sequence* rather than the name is what keeps non-ASCII input working:
  // `é` and `日` arrive with a name that is not a single Latin letter.
  //
  // The control test is a code-point comparison rather than a regex character
  // class: a literal `\x00-\x1f` class is both lint-flagged and easy to
  // mistranscribe, and this reads as what it means — C0 and DEL are the
  // terminal's escape machinery, everything else is text the composer should
  // receive [G5].
  const first = sequence.codePointAt(0) ?? 0;
  const isControl = first < 0x20 || first === 0x7f;
  const printable =
    !key.ctrl && !key.meta && sequence.length > 0 && !isControl
      ? sequence
      : undefined;
  return {
    name,
    ...(key.shift ? { shift: true } : {}),
    ...(key.ctrl ? { ctrl: true } : {}),
    ...(key.meta ? { meta: true } : {}),
    ...(printable !== undefined ? { char: printable } : {}),
  };
}

/**
 * The application shell.
 *
 * Below the floor `renderScreen` returns a single legible line and the app
 * **keeps running** — it does not exit and does not panic.
 */
export function Shell(props: ShellProps) {
  const dimensions = useTerminalDimensions();
  const clock = props.now ?? (() => Date.now());
  // §1.1's default selection applies at **boot**, not only on descent: "Home
  // therefore opens on your top mention when you have one." That is what makes
  // §4.4's jump-to-a-mention two keystrokes rather than five.
  const [state, setState] = createSignal<AppState>(
    withDefaultSelection(initialState(props.client.getSnapshot())),
  );

  onMount(() => {
    // One subscription to one stream (`daemon-api.md` §4.1). Re-reading the
    // whole snapshot per frame rather than patching state incrementally is
    // deliberate at this size: the daemon is authoritative, and a client-side
    // patch path would be a second state machine to keep in agreement with it.
    const unsubscribe = props.client.subscribe(() => {
      setState((previous) => ({
        ...previous,
        snapshot: props.client.getSnapshot(),
      }));
    });
    onCleanup(unsubscribe);
  });

  useKeyboard((key) => {
    const press = toKeyPress(key);

    // `ctrl+c` is the one binding outside the §5 chains, because it is the
    // terminal's contract rather than the app's — and §5.5 pins the behaviour
    // it must NOT have: mid-compose it clears the composer and does **not**
    // exit on the first press.
    if (press.ctrl && press.name === "c") {
      if (state().composer.length > 0) {
        setState((s) => ({ ...s, composer: "", cursor: 0 }));
        return;
      }
      props.onQuit();
      return;
    }

    const snapshot = state();
    const layer = current(snapshot.stack);
    const intent = resolveKey(
      {
        layer: layer.kind,
        composer: {
          text: snapshot.composer,
          cursor: snapshot.cursor,
          // Wave 1's composer is single-line, so §5.2's multiline branch is
          // vacuous and the chain falls through to the edge move — which is
          // exactly what it would do for a one-row multiline composer.
          multiline: false,
          cursorRow: 0,
          rowCount: 1,
        },
        surfaces: snapshot.surfaces,
        drawerOpen: snapshot.drawer !== null && !snapshot.drawer.expanded,
        drawerExpanded: snapshot.drawer?.expanded ?? false,
        messageSelect: snapshot.messageSelect !== null,
        // §5.1 rows 3 and 5 depend on which row is selected, which is a
        // function of the rendered row list. Deriving it in `screen.ts` and
        // reading it here is what keeps the key chain and the screen from
        // disagreeing about the selection.
        ...rowContext(snapshot),
      },
      press,
    );

    setState((s) => applyIntent(s, intent, dimensions().width, clock()));
  });

  const lines = createMemo(() =>
    renderScreen(state(), dimensions().width, dimensions().height, clock()),
  );

  return (
    <box style={{ flexDirection: "column", width: "100%", height: "100%" }}>
      <For each={lines()}>{(line) => <text>{line}</text>}</For>
    </box>
  );
}
