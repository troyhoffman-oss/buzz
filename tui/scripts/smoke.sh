#!/usr/bin/env bash
# Smoke-drive the TUI in a real PTY — the T2 pattern of DESIGN.md §5.5.
#
# `tmux` is a better *gate* than VHS: fast, headless, yields text — and it is
# exactly how the operators run the app, so the test harness and the product
# share a substrate.
#
# Rules from §5.5, each learned the hard way elsewhere and each applied here:
#   - Never `sleep N` as a readiness proxy. Poll `capture-pane` for a known
#     marker with a hard timeout. Blind sleeps are the number-one source of
#     flaky TUI CI.
#   - Fixed pane geometry at creation; never resize mid-test unless resize is
#     the thing under test.
#   - Isolated per-run `XDG_STATE_HOME` so the JSONL stores start empty and
#     parallel runs cannot corrupt each other.
#   - Always kill the session in a `trap` — orphaned tmux sessions on a VPS are
#     how this suite becomes a resource leak.
set -euo pipefail

cd "$(dirname "$0")/.."

SESSION="buzz-tui-smoke-$$"
STATE_DIR="$(mktemp -d)"
COLS="${SMOKE_COLS:-120}"
ROWS="${SMOKE_ROWS:-40}"
TIMEOUT_SECS="${SMOKE_TIMEOUT:-30}"

# `SMOKE_BIN=<path>` drives a compiled binary instead of the source tree, which
# is what the CI compile job asserts. Note the `-c "$STATE_DIR"`: a compiled
# binary re-reads whatever `bunfig.toml` is in its CWD and cannot resolve the
# dev-only preload from its embedded graph, so it must not run from `tui/`
# (see the comment in bunfig.toml).
# The fixture transport is the only transport until the daemon attach path
# lands (§2.3), and it is a *product* path rather than a test hook — §5.5's
# whole point is that the harness drives the shipped binary. An absolute path,
# because a compiled binary runs from a scratch directory (below).
FIXTURE="${SMOKE_FIXTURE:-$PWD/fixtures/seeded-basic.jsonl}"

if [[ -n "${SMOKE_BIN:-}" ]]; then
  LAUNCH="$(cd "$(dirname "$SMOKE_BIN")" && pwd)/$(basename "$SMOKE_BIN")"
  LAUNCH_CWD="$STATE_DIR"
else
  LAUNCH="bun run src/main.ts"
  LAUNCH_CWD="$PWD"
fi

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  rm -rf "$STATE_DIR"
}
trap cleanup EXIT

tmux new-session -d -s "$SESSION" -c "$LAUNCH_CWD" -x "$COLS" -y "$ROWS" \
  "XDG_STATE_HOME=$STATE_DIR BUZZ_TUI_FIXTURE=$FIXTURE \
   BUZZ_TUI_FIXED_TIME=2026-08-04T14:12:00Z BUZZ_TUI_NO_ANIM=1 \
   TZ=UTC LANG=C.UTF-8 $LAUNCH"

# `-x/-y` alone is NOT enough, and getting this wrong is silent. tmux's default
# `window-size latest` resizes a session to whatever client attached most
# recently, so on a box that is already running tmux — which is this product's
# entire premise (§1.2) — the requested geometry is discarded and the pane
# inherits the other session's size. A snapshot suite built on that would pin
# assertions to the developer's terminal width rather than to the tier under
# test. `manual` plus an explicit resize is what makes §5.5's "fixed pane
# geometry at creation" true rather than requested.
tmux set-option -t "$SESSION" window-size manual
tmux resize-window -t "$SESSION" -x "$COLS" -y "$ROWS"

actual="$(tmux list-panes -t "$SESSION" -F '#{pane_width}x#{pane_height}' | head -1)"
if [[ "$actual" != "${COLS}x${ROWS}" ]]; then
  echo "::error::pane geometry is $actual, expected ${COLS}x${ROWS}" >&2
  exit 1
fi

# Poll for the frame rather than sleeping. The statusline's relay row is the
# readiness marker: it is the last band drawn and it never drops at any width
# (NAVIGATION.md §2.1), so it stays valid as layers land on top of this frame.
deadline=$((SECONDS + TIMEOUT_SECS))
until tmux capture-pane -p -t "$SESSION" | grep -q 'buzz://'; do
  if [[ $SECONDS -ge $deadline ]]; then
    echo "::error::TUI did not render the shell within ${TIMEOUT_SECS}s" >&2
    echo "--- final pane ---" >&2
    tmux capture-pane -p -t "$SESSION" >&2 || true
    exit 1
  fi
  sleep 0.1
done

pane="$(tmux capture-pane -p -t "$SESSION")"

# The bands of §2, plus the L0 zones. There are no rail/list/main/aux regions
# any more — §7 supersedes that shell outright, and asserting on them would
# keep a gate green against a layout the design deleted.
#
# `❯` is the [G8] check in its cheapest form: exactly one focus glyph, which is
# the invariant most likely to break silently when a new surface is added.
for marker in 'ATTENTION' 'PLACES' 'buzz://' '⏵'; do
  if ! grep -q "$marker" <<<"$pane"; then
    echo "::error::'$marker' missing from the frame at ${COLS}x${ROWS}" >&2
    echo "$pane" >&2
    exit 1
  fi
done

glyphs="$(grep -o '❯' <<<"$pane" | wc -l | tr -d ' ')"
if [[ "$glyphs" != "1" ]]; then
  echo "::error::[G8] expected exactly one ❯ on screen, found $glyphs" >&2
  echo "$pane" >&2
  exit 1
fi

# §1.1's default selection: home opens on the top mention when there is one.
# This is what makes §4.4's jump two keystrokes, and it is worth asserting in a
# real PTY because it is a boot-order property the unit tests reach differently.
if ! grep -q '❯ matt' <<<"$pane"; then
  echo "::error::home did not open on the top mention (§1.1)" >&2
  echo "$pane" >&2
  exit 1
fi

# Quits cleanly on ctrl+c with an empty composer (§5.5: mid-compose it clears
# the composer and does NOT exit on the first press — that case is a T2
# sequence, not this smoke).
tmux send-keys -t "$SESSION" C-c

deadline=$((SECONDS + TIMEOUT_SECS))
while tmux has-session -t "$SESSION" 2>/dev/null; do
  if [[ $SECONDS -ge $deadline ]]; then
    echo "::error::TUI did not exit within ${TIMEOUT_SECS}s of ctrl+c" >&2
    exit 1
  fi
  sleep 0.1
done

echo "smoke ok: shell rendered at ${COLS}x${ROWS} and quit cleanly on ctrl+c"
