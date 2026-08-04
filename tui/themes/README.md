# Themes

Catppuccin, per DESIGN.md §3.10: **Latte (light) and Macchiato (dark) as one
theme with two modes**, matching the desktop and mobile apps.

## Why the palettes are JSON and not TypeScript

`just tui-check-boundary` fails the build on any `#rrggbb` literal under
`src/` (§6.4, §3.10's "no literal hex in feature code"). That gate is blunt on
purpose — a narrowed version of its sibling rule already let a real event kind
through once. Keeping the palettes here as **data** makes the rule absolute
instead of allowlisted, and gives the legitimate colour values one auditable
home. `src/theme/theme.ts` loads and validates them against the closed token
set at module load, so a missing token is a startup error rather than an
`undefined` that surfaces at one call site under one condition.

## Deviations from stock Catppuccin, and why

§3.10 makes **WCAG AA contrast a CI test**, not a review note:

> WCAG AA contrast is a CI test over every (foreground, background) pair in the
> token table, per theme, in both modes.

`test/unit/theme.test.ts` is that test. Macchiato passes stock. **Latte does
not**: measured against `backgroundElement` (`#ccd0da`, Latte's `surface0`,
which is the darkest surface text is drawn on), every stock accent lands
between 2.06:1 and 3.62:1 — all below the 4.5:1 threshold for normal-size text.

That is not a theoretical failure. It is the exact defect a palette review
cannot catch, because the text is present, the layout is right, and it simply
cannot be read by the person using the mode the reviewer was not in.

The fix scales each accent toward black along its own hue until it clears 4.5:1,
which preserves the colour's identity while making it legible:

| Token | Stock Latte | Here | Ratio on `#ccd0da` |
|---|---|---|---|
| `primary` | `#1e66f5` | `#1851c2` | 4.54 |
| `secondary` | `#7287fd` | `#46529a` | 4.64 |
| `accent` | `#8839ef` | `#7230c9` | 4.56 |
| `error` | `#d20f39` | `#b20d30` | 4.54 |
| `warning` | `#df8e1d` → `#864b00` | `#864b00` | 4.50 |
| `success` | `#40a02b` → `#27661a` | `#27661a` | 4.53 |
| `info` | `#179299` → `#096272` | `#096272` | 4.53 |
| `textMuted` | `#6c6f85` | `#555869` | 4.56 |

`borderActive` follows `accent`, and the `diff*` family follows the semantic
token it mirrors, so the two never drift apart.

**Border tokens are deliberately exempt from the AA loop.** They draw rules and
box chrome, not glyphs you read, and 4.5:1 is a *text* threshold. Holding a
hairline to it would force the chrome to compete with the content — precisely
what §3.10's "neutral tones for large surfaces and chrome; high-chroma accents
reserved for focus and selection" rules out. Their requirement is instead that
they be distinguishable from the surface without shouting, asserted as a band
(>1.5:1, <9:1) in the same test.

## Adding a token

1. Add the name to `TOKEN_NAMES` in `src/theme/tokens.ts` — the set is closed,
   and that list is what makes it so.
2. Add the value to **both** palettes. `theme.ts` throws at load if either is
   missing it, which is the failure you want: loud, at startup, naming the token.
3. If it carries text, add it to `TEXT_TOKENS` in `test/unit/theme.test.ts` so
   it is held to AA in both modes.

## `probe.json`

Scaffolding scratch used by `test/render/probe.test.tsx` while bringing up the
OpenTUI renderer. Not part of the theme.
