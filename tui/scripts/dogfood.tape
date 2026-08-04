# VHS tape — a recorded walkthrough of the Wave-1 TUI.
#
# Renders the same key sequence the tmux captures assert, as a GIF. VHS is the
# §5.6 artifact; tmux stays the gate (fast, headless, text-diffable). This is
# for humans, not for CI.
#
#   vhs scripts/dogfood.tape
#
# Env is set on the Type line rather than via `Env` so the tape runs against the
# fixture transport exactly as the tmux harness does.

Output dogfood-walkthrough.gif

Set Shell "bash"
Set FontSize 13
Set Width 1500
Set Height 800
Set Padding 12
Set Framerate 24
Set PlaybackSpeed 1.0

# Boot at L0 HOME.
Type "BUZZ_TUI_FIXTURE=$PWD/fixtures/seeded-basic.jsonl BUZZ_TUI_FIXED_TIME=2026-08-04T14:12:00Z BUZZ_TUI_NO_ANIM=1 TZ=UTC bun run src/main.ts"
Enter
Sleep 6s

# L0: the attention groups, with the default selection already applied (§1.1).
Sleep 2s

# §4.4 — two keystrokes from boot to the mentioned message.
Right
Sleep 3s

# §5.2 — into message-select, then leap to the previous thread root.
Up
Sleep 2s
Up@300ms 2
Sleep 1s

# Descend into the thread (L3).
Right
Sleep 3s

# Back out to the channel.
Escape
Sleep 2s

# §3.3 — the mention picker over the resolved directory.
Type "@"
Sleep 3s
Escape
Sleep 1s

# §2.4 — the drawer peek: agent activity without stealing focus.
Down
Sleep 3s
Escape
Sleep 1s

# §3.5 — ctrl+k search, hits render as the query is typed.
Ctrl+K
Sleep 1s
Type@120ms "read"
Sleep 4s
