# T2 — tmux drive (also the dogfood loop)

Implements `DESIGN.md` §5.5. **Empty until the fixture transport lands**
(Wave 1, §5.4), and stated rather than silent: `bun test` on an empty directory
exits 0, so an unwritten suite is indistinguishable from a passing one.
`scripts/smoke.sh` is the one T2-shaped check that exists today —
`just tui-smoke` boots the shell in a real PTY and asserts it renders and quits.

`tmux` is a better *gate* than VHS: fast, headless, yields text — and it is
exactly how the operators run the app, so the test harness and the product
share a substrate.

Rules, each learned the hard way elsewhere:

- **Never `sleep N` as a readiness proxy** — poll `capture-pane` for a known
  marker with a hard timeout. Blind sleeps are the number-one source of flaky
  TUI CI.
- **Fixed pane geometry at creation.** `-x/-y` alone is **not** enough: tmux's
  default `window-size latest` resizes a session to whatever client attached
  most recently, so on a box already running tmux — this product's own premise
  (§1.2) — the requested geometry is silently discarded. Use
  `set-option window-size manual` plus an explicit `resize-window`, then
  **assert** the resulting pane size before capturing. `scripts/smoke.sh` does
  all three.
- **Isolated per-run `XDG_STATE_HOME`** so the JSONL stores (frecency, history,
  drafts) start empty and parallel runs cannot corrupt each other.
- **`capture-pane -e`** when the assertion is about colour.
- **`capture-pane -p -S -`** for scrollback assertions.
- **Always kill the session in a `trap`** — orphaned tmux sessions on a VPS are
  how this suite becomes a resource leak.

The gated sequences to write are tabulated in §5.5 — among them: two TUIs
launched within 50 ms yield exactly one daemon pid (§2.3); resize
120→72→50→120 walks MD → MD-narrow → XS → MD with unread counts retained at 72;
`ctrl+c` mid-compose clears the composer and does **not** exit on first press;
a draft composed in pane 1 appears in pane 2, because drafts are daemon-held.
