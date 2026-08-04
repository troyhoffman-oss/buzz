/**
 * The four-region shell — DESIGN.md §3.0.
 *
 * ```
 * ┌ rail ┬ list ─────────┬ main ──────────────────────────┬ aux ───────────┐
 * │  ●   │ channels /    │  timeline / transcript /       │ thread /       │
 * │  ○   │ inbox filters │  search results / fleet        │ members /      │
 * │  ○   │               │                                │ agent detail   │
 * ├──────┴───────────────┴────────────────────────────────┴────────────────┤
 * │ composer (main-scoped)                                                  │
 * ├─────────────────────────────────────────────────────────────────────────┤
 * │ status bar: links · pending-leader · counts · context                   │
 * └─────────────────────────────────────────────────────────────────────────┘
 * ```
 *
 * Every screen is this shell. Only the middle changes. Regions collapse by
 * responsive tier (§3.9), **never by squeezing**. The status bar and composer
 * are the last things to go, and neither ever does.
 *
 * Wave-1 scaffold: the panes render labelled placeholders. The screens that
 * fill them (§4.1.2 — chat, thread, mention picker, fleet, activity, search,
 * palette) land on top of this frame.
 */

import { useKeyboard, useTerminalDimensions } from "@opentui/solid";
import { Show, createMemo } from "solid-js";
import { MIN_COLS, MIN_ROWS, isBelowFloor, layoutFor, tierFor } from "./tiers";

/** Props for {@link Shell}. */
export interface ShellProps {
  /** Called when the user quits. */
  onQuit: () => void;
}

/**
 * The application shell.
 *
 * Below the §3.9 floor it renders a single legible line and **keeps running** —
 * rendering into a zero-size rect is a no-op, never a panic.
 */
export function Shell(props: ShellProps) {
  const dimensions = useTerminalDimensions();
  const tier = createMemo(() => tierFor(dimensions().width));
  const layout = createMemo(() => layoutFor(dimensions().width));
  const tooSmall = createMemo(() =>
    isBelowFloor(dimensions().width, dimensions().height),
  );

  useKeyboard((key) => {
    // Wave-1 scaffold binding. §3.7 makes every advertised key resolve through
    // a declarative table rather than a hardcoded handler, and §6.4 gates that
    // mechanically — so this is the one place a literal key survives, and only
    // until the keybind table lands.
    if (key.name === "q") props.onQuit();
  });

  return (
    <Show
      when={!tooSmall()}
      fallback={
        <text>{`terminal too small — ${MIN_COLS}x${MIN_ROWS} minimum`}</text>
      }
    >
      <box style={{ flexDirection: "column", width: "100%", height: "100%" }}>
        {/* rail + list + main + aux */}
        <box style={{ flexDirection: "row", flexGrow: 1 }}>
          <Show when={layout().rail}>
            <box border title="rail" style={{ width: 6 }}>
              <text>{"●\n○\n○"}</text>
            </box>
          </Show>

          <Show when={layout().list !== "none" && layout().list !== "dialog"}>
            <box
              border
              title="list"
              style={{ width: layout().list === "collapsed" ? 18 : 24 }}
            >
              <text>
                {layout().list === "collapsed"
                  ? "# ●9\n#\n# ★"
                  : "CHANNELS\nTHREADS\nDMS\nAGENTS"}
              </text>
            </box>
          </Show>

          <box border title="main" style={{ flexGrow: 1 }}>
            <text>{`timeline · tier ${tier()} · ${dimensions().width}x${dimensions().height}`}</text>
          </box>

          <Show when={layout().aux === "pane"}>
            <box border title="aux" style={{ width: 24 }}>
              <text>thread / members</text>
            </box>
          </Show>
        </box>

        {/* The composer and status bar never drop, at any tier (§3.0). */}
        <box border title="composer" style={{ height: 3 }}>
          <text>{"> "}</text>
        </box>
        <box style={{ height: 1 }}>
          <text>{"⬤⬤ │ ^X- │ q quit"}</text>
        </box>
      </box>
    </Show>
  );
}
