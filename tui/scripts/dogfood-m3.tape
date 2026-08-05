# VHS tape — the M3 walkthrough, against a **live daemon**.
#
# The M1 tape (`dogfood.tape`) records the fixture transport, because at M1 a
# live daemon served nothing. This one drives the compiled binary attached over
# `BUZZ_DAEMON_SOCKET` to a real `buzz-daemon` holding the real claude-test
# identity against the real relay — so what it records is a real Buzz
# conversation moving through the layers, not a scenario file.
#
#   BUZZ_M3_SOCKET=/path/to/daemon.sock vhs scripts/dogfood-m3.tape
#
# tmux stays the gate (fast, headless, text-diffable); this is the §5.6 artifact
# for humans.
#
# **Read-only.** Every keystroke below is a descend, a select, an expand, or a
# search. Nothing presses Enter on a non-empty composer, which is the only
# keystroke that sends. The identity is a shared test agent.
#
# The binary is run from `$HOME` rather than from `tui/`: a compiled Bun binary
# re-reads `bunfig.toml` from its CWD, and `tui/bunfig.toml` names a preload the
# embedded module graph cannot resolve — running it from inside `tui/` dies with
# `preload not found` and reads as a broken release build.

Output dogfood-m3-live.gif

Set Shell "bash"
Set FontSize 13
Set Width 1500
Set Height 800
Set Padding 12
Set Framerate 24
Set PlaybackSpeed 1.0

# Boot at L0 HOME, attached to the live daemon.
Type "cd $HOME && BUZZ_DAEMON_SOCKET=$BUZZ_M3_SOCKET BUZZ_TUI_NO_ANIM=1 TZ=UTC $BUZZ_M3_BINARY"
Enter
Sleep 8s

# L0: real relay host, real pubkey, real unread and mention totals.
Sleep 3s

# L1 CHANNELS — the relay's own membership answer, found by the cold-start
# discovery walk. This row is the one M2 could not produce.
Right
Sleep 4s

# L2 — a real conversation from a real relay: messages, a date divider, a
# thread count, and a dimmed system row.
Right
Sleep 5s

# §5.2 — up into message-select. The ❯ leaves the composer for the timeline.
Up
Sleep 3s
Up@400ms 2
Sleep 2s

# Back to the composer.
Escape
Sleep 2s

# §2.3 — the drawer peek: live agent activity without stealing focus.
Down
Sleep 4s
Escape
Sleep 2s

# §3.5 — ctrl+k search against the daemon's real index.
Ctrl+K
Sleep 2s
Type@120ms "read"
Sleep 4s
Escape
Sleep 2s

# ctrl+g home, and out.
Ctrl+G
Sleep 3s
Ctrl+C
Sleep 1s
