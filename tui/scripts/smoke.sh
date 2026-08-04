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
  "XDG_STATE_HOME=$STATE_DIR BUZZ_TUI_NO_ANIM=1 TZ=UTC LANG=C.UTF-8 $LAUNCH"

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

# Poll for the shell frame rather than sleeping. `composer` is a §3.0 region
# that never drops at any tier, so it is a readiness marker that stays valid as
# the screens land on top of this frame.
deadline=$((SECONDS + TIMEOUT_SECS))
until tmux capture-pane -p -t "$SESSION" | grep -q 'composer'; do
  if [[ $SECONDS -ge $deadline ]]; then
    echo "::error::TUI did not render the shell within ${TIMEOUT_SECS}s" >&2
    echo "--- final pane ---" >&2
    tmux capture-pane -p -t "$SESSION" >&2 || true
    exit 1
  fi
  sleep 0.1
done

pane="$(tmux capture-pane -p -t "$SESSION")"

# Every §3.0 region that this tier draws must be present. At 120 cols the tier
# is LG: list + main + aux, no rail (§3.9).
for region in list main aux composer; do
  if ! grep -q "$region" <<<"$pane"; then
    echo "::error::shell region '$region' missing at ${COLS}x${ROWS}" >&2
    echo "$pane" >&2
    exit 1
  fi
done

# Quits cleanly on `q`.
tmux send-keys -t "$SESSION" 'q'

deadline=$((SECONDS + TIMEOUT_SECS))
while tmux has-session -t "$SESSION" 2>/dev/null; do
  if [[ $SECONDS -ge $deadline ]]; then
    echo "::error::TUI did not exit within ${TIMEOUT_SECS}s of 'q'" >&2
    exit 1
  fi
  sleep 0.1
done

echo "smoke ok: shell rendered at ${COLS}x${ROWS} and quit cleanly on q"
