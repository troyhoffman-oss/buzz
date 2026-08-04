# T1 — headless render snapshots

Implements `DESIGN.md` §5.3. **Empty until the harness lands** (Wave 1,
§4.1.2), and deliberately so rather than silently: `bun test` on an empty
directory exits 0, so an unwritten suite and a passing suite look identical in
CI. `test/render/harness.test.ts` fails until the real matrix replaces it.

Render the component tree to an in-memory cell buffer at fixed dimensions;
snapshot the **text grid**, not pixels. `@opentui/solid` exports `testRender`
for exactly this.

Six determinism requirements — a snapshot suite without all six is worthless:

1. **Frozen clock** — `BUZZ_TUI_FIXED_TIME`, all timestamps from an injected clock.
2. **Frozen randomness** — seeded PRNG for anything sampling.
3. **No animation** — `BUZZ_TUI_NO_ANIM=1` pins spinners to frame 0.
4. **Fixed dimensions** per snapshot, declared in the test name.
5. **Fixed theme, `LANG=C.UTF-8`, `TZ=UTC`.**
6. **Fixture-backed daemon** — T1 never touches a network.

Plus a **shadow-run** check: two runs of the same input must produce
byte-identical buffers.

Matrix: every screen × every tier × both glyph-width policies.

```
snapshots/<screen>/<tier>[.ambiguous-wide].txt
  tiers: xs(50x20) sm(70x24) mdn(80x28) md(100x30) lg(140x40) xl(180x50)
```

Three matrix notes from §5.3, each a trap:

- The `xs`/`sm` rows are the ones everyone skips and they are the *phone* rows
  here (§1.2).
- `usage-strip` exists at **every** tier including `xs`; `usage-pane` exists
  only where aux does (`md` and up). A `usage-pane × xs` snapshot would be a
  snapshot of undefined behaviour.
- `agent-permission` is named `agent-ask`, matching §3.4.1's actual mechanism,
  and its fixture covers both routings.

`BLESS=1 just tui-test-render` rewrites; the diff is reviewed like any other
diff.
