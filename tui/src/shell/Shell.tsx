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
import {
  For,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import { applyIntent, withDefaultSelection } from "../app/dispatch";
import { renderScreen, rowContext } from "../app/screen";
import { type AppState, initialState, takeEffect } from "../app/state";
import type { DaemonClient } from "../client/daemon-client";
import { type KeyPress, resolveKey } from "../nav/keys";
import { current } from "../nav/layers";
import { resolveTheme } from "../theme/theme";
import { color } from "../theme/tokens";

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

    // The reducer decides on effects but cannot perform them — it is pure. The
    // shell is the drain, and it is the **only** drain: `takeEffect` clears the
    // slot as it hands the effect over, so a subsequent keystroke cannot
    // re-send the same message.
    //
    // Without this the whole write path is dead in a way that reads as working:
    // `⏎` clears the composer (the reducer did that part), so the keystroke
    // *looks* accepted, and the message is simply gone. Nothing errors and
    // nothing is logged — the exact §1.3-property-3 failure the rest of this
    // design works to prevent, reached through the one place effects leave the
    // pure core.
    // Reduced outside the setter rather than inside an updater function: an
    // updater that assigns to an enclosing variable is a side effect in what
    // is supposed to be a pure state transition, and it only works at all
    // because Solid happens to call it synchronously. `snapshot` is the state
    // read at the top of this handler, and a key handler is synchronous, so
    // there is nothing to race with.
    const next = applyIntent(snapshot, intent, dimensions().width, clock());
    const [effect, cleared] = takeEffect(next);
    setState(cleared);
    if (effect) void perform(effect);
  });

  /**
   * Perform a drained effect against the daemon.
   *
   * Deliberately fire-and-forget from the key handler's perspective: the daemon
   * is authoritative and every screen re-reads the snapshot on the next stream
   * frame, so the sent message arrives back through the same path a message
   * from anyone else does. Optimistically inserting it here would be a second
   * state machine to keep in agreement with that one, and [D-7]'s
   * provisional-id correlation is the daemon's job.
   *
   * **A rejected send must not be swallowed.** Dropping the rejection would
   * reproduce, one layer down, the exact bug this drain exists to fix: the
   * composer clears, nothing appears, and nothing says why. Restoring the text
   * is the honest minimum — the operator keeps what they wrote and can see it
   * did not go. Wave 1 has no error surface in `AppState` to render a reason
   * into; when one lands, this is the one place that has the reason.
   */
  async function perform(
    effect: NonNullable<ReturnType<typeof takeEffect>[0]>,
  ) {
    switch (effect.kind) {
      case "send":
        try {
          await props.client.send(effect.channelId, effect.content, {
            ...(effect.replyTo ? { replyTo: effect.replyTo } : {}),
            mentions: effect.mentions,
          });
        } catch {
          // Only restore into an untouched composer: the send is async, and
          // clobbering a message the operator has since started writing would
          // be a worse betrayal than the one being repaired.
          setState((s) =>
            s.composer.length === 0
              ? {
                  ...s,
                  composer: effect.content,
                  cursor: effect.content.length,
                  mentions: effect.mentions,
                }
              : s,
          );
          // Swallowed **deliberately**, and this is the one place it is right
          // to. `perform` is called fire-and-forget from a key handler, so a
          // rethrow is an unhandled rejection — which under Bun's default can
          // take the process down, turning a failed message into a lost
          // session. Nor can the reason go to stderr: this process owns the
          // terminal, and writing to it corrupts the frame.
          //
          // The restored text is therefore the whole signal, and it is a real
          // one: the operator sees their message did not go and still has it.
          // A *reason* needs an error surface in `AppState`, which Wave 1 does
          // not have; when it lands, this catch is where it gets filled in.
        }
        return;
      case "markRead":
        await props.client.markRead(effect.channelId);
        return;
      case "markAllRead":
        // `markAllRead` is a fan-out over every channel that has unread, which
        // the four-method client expresses as one `markRead` each rather than
        // as a fifth method. Reading the channel list here rather than in the
        // reducer keeps the effect a description of intent, not of traffic.
        await Promise.all(
          state()
            .snapshot.channels.filter((channel) => channel.unread > 0)
            .map((channel) => props.client.markRead(channel.id)),
        );
        return;
    }
  }

  /**
   * Load a channel's timeline when one becomes the current layer.
   *
   * Declarative rather than threaded through the key handler, because a channel
   * layer is reachable by four routes — `→` from L1, §4.4's teleport from a
   * home attention row, a `ctrl+k` search hit, and `←` back onto one already on
   * the stack. Hooking descent would cover the first and miss the rest, and the
   * miss is invisible: the timeline renders empty, which is exactly what an
   * empty channel looks like.
   *
   * On the fixture transport `ensureMessages` is a no-op, so this costs
   * nothing there; on the socket transport it is idempotent, so `←` `→`
   * between two channels does not re-query.
   *
   * The rejection is swallowed for the reason `perform`'s catch documents at
   * length: this runs outside any handler that could receive a throw, an
   * unhandled rejection can take the process down under Bun, and stderr belongs
   * to the renderer. The `missing[]` channel the client already maintains is
   * what carries the loss to the screen (§1.3 property 3).
   */
  createEffect(() => {
    const layer = current(state().stack);
    if (layer.kind !== "channel" || !layer.channelId) return;
    const channelId = layer.channelId;
    void props.client
      .ensureMessages(channelId)
      .then(() =>
        setState((s) => ({ ...s, snapshot: props.client.getSnapshot() })),
      )
      .catch(() => {});
  });

  const lines = createMemo(() =>
    renderScreen(state(), dimensions().width, dimensions().height, clock()),
  );

  // §3.10: "pre-compute styles once at startup." The theme is resolved from
  // the environment a single time rather than per frame — it cannot change
  // without a relaunch, and re-reading `process.env` on every render would put
  // an environment lookup in the hot path of a streaming chat surface.
  const theme = resolveTheme();
  const fg = color(theme, "text");
  const bg = color(theme, "background");

  // TODO(wave1, §3.10): only the base pair is applied. Per-token colouring —
  // the [G12] ladder's state tones, `diff*` on diff rows, the deterministic
  // per-user hue, the per-community accent — needs the renderers to emit
  // *spans* rather than plain strings, which is a change to the `string[]`
  // contract `renderScreen` and the whole T1 matrix are built on. Doing it
  // half-way (colouring only what is easy to reach from here) would leave the
  // token set looking applied while most of §3.10's disciplines were not, so
  // it lands as one change with its own snapshot pass rather than as a
  // sprinkle. `NO_COLOR` and `TERM=dumb` are honoured today because `color()`
  // returns `undefined` and OpenTUI inherits the terminal's own colours.
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
