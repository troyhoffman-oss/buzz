# Buzz TUI

A terminal client for Buzz that runs where the agents run. Architecture in ten
lines:

1. Two processes: a Bun/OpenTUI front end (`tui/`) and a Rust `buzz-daemon`
   (`crates/buzz-daemon/`), talking HTTP/1.1 + ndjson over a Unix socket.
2. The **daemon owns all protocol knowledge** — keys, NIP-42, subscriptions,
   reconnect, cache — and reuses `buzz-ws-client` and `buzz-sdk` verbatim.
3. The **front end is deliberately disposable**: its whole network layer is one
   HTTP client and one line reader. It never parses an event, sees a key, knows
   a relay URL, or learns a kind number. `just tui-check-boundary` enforces it.
4. One daemon per (relay, identity), keyed by
   `sha256(relay:pubkey:auth_tag_owner)`; multi-community is an N-daemon fan-out
   in the TUI, never multi-tenancy in the daemon.
5. Keys live at rest as NIP-49 `ncryptsec`; only the *passphrase* ever crosses a
   process boundary, on stdin. Socket authorization is `0600` plus a
   peer-credential check. No TCP listener ships.
6. The user runs `buzz-tui`. The daemon is spawned for them and survives Ctrl-C.
7. Every screen is one four-region shell — rail / list / main / aux, plus a
   composer and status bar that never drop at any width.
8. Loss is always visible: dropped frames, gapped cursors, an unreachable relay,
   and a keyless daemon are all rendered states, never silence.
9. Tests assert on text — units, headless grid snapshots, and `tmux`-driven
   PTY runs. Video is evidence for humans, never an oracle.
10. Full desktop parity is the destination, shipped in waves ordered by operator
    pain rather than module size.

The design of record is [`docs/tui/DESIGN.md`](../docs/tui/DESIGN.md); every
module in this package and in `crates/buzz-daemon/` cites the section it
implements.

## Getting started

Requires [Bun](https://bun.sh/install) (`curl -fsSL https://bun.sh/install | bash`).

```bash
just tui-install        # bun install
just tui-dev            # run against a live daemon
just tui-test-unit      # T0 — pure units, no terminal
just tui-smoke          # boot in a real PTY, assert the shell, quit on q
just tui-check          # typecheck + lint
just tui-check-boundary # the disposability gate (§6.4)
just tui-check-mocks    # every mock in DESIGN.md measures what its label claims
just tui-build          # compile a single binary for one triple
```

`tui/` is **not** a pnpm workspace member, by design (§6.1): Bun and pnpm do not
share a lockfile graph here, and `@opentui/core`'s prebuilt platform binaries
stay out of pnpm's symlinked store. `just tui-*` is the entry point.

Two things that look like component bugs but are not, both commented where they
bite: without `bunfig.toml`'s preload, Bun resolves `solid-js` to its SSR build
and the first render dies with "Orphan text error"; and `bun build --compile`
does not apply the Solid transform, so releases go through `scripts/build.ts`
instead ([anomalyco/opentui#122](https://github.com/anomalyco/opentui/issues/122)).
