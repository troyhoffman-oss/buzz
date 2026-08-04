/**
 * Theme — DESIGN.md §3.10, which §7 leaves unaffected.
 *
 * > **WCAG AA contrast is a CI test** over every (foreground, background) pair
 * > in the token table, per theme, in both modes.
 *
 * That sentence is why this file exists rather than a review note. A palette is
 * exactly the kind of artifact where "looks fine on my terminal" passes review
 * and fails for the person using the other mode, and the failure is invisible
 * to every other gate in the package: the text is there, the layout is right,
 * and it simply cannot be read.
 */

import { describe, expect, test } from "bun:test";
import { PALETTES, resolveTheme } from "../../src/theme/theme";
import {
  AA_NORMAL,
  TOKEN_NAMES,
  type ThemeMode,
  type TokenName,
  color,
  contrastRatio,
  isColorless,
  modeFromEnv,
  userHueIndex,
} from "../../src/theme/tokens";

/** Surfaces text is drawn on. Every foreground must clear AA against each. */
const SURFACES: readonly TokenName[] = [
  "background",
  "backgroundPanel",
  "backgroundElement",
  "backgroundMenu",
];

/**
 * Foregrounds that carry **text**, and therefore owe AA at normal size.
 *
 * Border tokens are deliberately excluded: they draw rules and box chrome, not
 * glyphs you read, and WCAG's 4.5:1 is a *text* threshold. Holding a hairline
 * to it would force the chrome to compete with the content for attention —
 * which is precisely what §3.10's "neutral tones for large surfaces and chrome"
 * rules out. Their own (lower) requirement is that they be distinguishable from
 * the surface at all, asserted separately below.
 */
const TEXT_TOKENS: readonly TokenName[] = [
  "primary",
  "secondary",
  "accent",
  "error",
  "warning",
  "success",
  "info",
  "text",
  "textMuted",
  "diffAdded",
  "diffRemoved",
  "diffContext",
  "diffHeader",
];

describe("§3.10 WCAG AA contrast, per theme, in both modes", () => {
  for (const mode of ["light", "dark"] as ThemeMode[]) {
    for (const fg of TEXT_TOKENS) {
      for (const bg of SURFACES) {
        test(`${mode}: ${fg} on ${bg}`, () => {
          const palette = PALETTES[mode];
          const ratio = contrastRatio(palette[fg], palette[bg]);
          expect(ratio).toBeGreaterThanOrEqual(AA_NORMAL);
        });
      }
    }
  }

  test("selected-row text clears AA against the accent it sits on", () => {
    // The one pair the surface loop cannot cover: `selectedListItemText` is
    // drawn on `accent`, not on a background token. It is also the pair most
    // likely to be wrong, because the accent is chosen for chroma.
    for (const mode of ["light", "dark"] as ThemeMode[]) {
      const palette = PALETTES[mode];
      expect(
        contrastRatio(palette.selectedListItemText, palette.accent),
      ).toBeGreaterThanOrEqual(AA_NORMAL);
    }
  });

  test("borders are visible against their surfaces without competing", () => {
    // §3.10: "Neutral tones for large surfaces and chrome; high-chroma accents
    // reserved for focus and selection." A border must be *seen* (>1.5:1) but
    // must not shout — a rule with text-grade contrast reads as content.
    for (const mode of ["light", "dark"] as ThemeMode[]) {
      const palette = PALETTES[mode];
      for (const border of ["border", "borderSubtle"] as TokenName[]) {
        const ratio = contrastRatio(palette[border], palette.background);
        expect(ratio).toBeGreaterThan(1.5);
        expect(ratio).toBeLessThan(AA_NORMAL * 2);
      }
    }
  });
});

describe("the token set is closed", () => {
  test("both palettes define exactly the declared tokens, and no more", () => {
    // A palette missing a token surfaces as `undefined` at one call site under
    // one condition — the worst shape of theme bug, because it looks like a
    // rendering glitch rather than a missing value. `theme.ts` throws at load;
    // this asserts the other direction too.
    for (const mode of ["light", "dark"] as ThemeMode[]) {
      const keys = Object.keys(PALETTES[mode]).sort();
      expect(keys).toEqual([...TOKEN_NAMES].sort());
    }
  });
});

describe("§3.10 NO_COLOR and TERM=dumb are honoured", () => {
  test("NO_COLOR disables colour for ANY value, including empty", () => {
    // The informal standard is presence, not truthiness: `NO_COLOR=` is the
    // documented way to say yes, and a truthiness check re-enables colour for
    // exactly the users who asked hardest for it to be off.
    expect(isColorless({ NO_COLOR: "" })).toBe(true);
    expect(isColorless({ NO_COLOR: "1" })).toBe(true);
    expect(isColorless({ NO_COLOR: "0" })).toBe(true);
    expect(isColorless({})).toBe(false);
  });

  test("TERM=dumb disables colour", () => {
    expect(isColorless({ TERM: "dumb" })).toBe(true);
    expect(isColorless({ TERM: "xterm-256color" })).toBe(false);
  });

  test("a colourless theme resolves every token to undefined, not to white", () => {
    // `undefined` means "inherit the terminal's own foreground", which is what
    // NO_COLOR asks for. Substituting white would override a light terminal
    // with unreadable text — a worse outcome than the colour it replaced.
    const theme = resolveTheme({ NO_COLOR: "1" });
    for (const token of TOKEN_NAMES) {
      expect(color(theme, token)).toBeUndefined();
    }
  });
});

describe("§3.10 mode resolution", () => {
  test("BUZZ_TUI_THEME is the explicit lock", () => {
    expect(modeFromEnv({ BUZZ_TUI_THEME: "light" })).toBe("light");
    expect(modeFromEnv({ BUZZ_TUI_THEME: "dark" })).toBe("dark");
    // The lock beats the terminal's own report.
    expect(modeFromEnv({ BUZZ_TUI_THEME: "dark", COLORFGBG: "0;15" })).toBe(
      "dark",
    );
  });

  test("COLORFGBG's background field is the terminal's report", () => {
    expect(modeFromEnv({ COLORFGBG: "0;15" })).toBe("light");
    expect(modeFromEnv({ COLORFGBG: "15;0" })).toBe("dark");
  });

  test("an unreported terminal defaults to dark", () => {
    // A VPS terminal is overwhelmingly dark, and light-on-light is the less
    // recoverable of the two wrong guesses.
    expect(modeFromEnv({})).toBe("dark");
    expect(modeFromEnv({ COLORFGBG: "garbage" })).toBe("dark");
  });
});

describe("§3.10 per-user colour is deterministic", () => {
  test("the same pubkey yields the same hue on any machine", () => {
    // "Deterministic, so the same person is the same colour on both operators'
    // boxes." A `Math.random` or an insertion-order index would make the two
    // operators' screens disagree about who is who.
    const pubkey = "pk_troy_00000000000000000000000000000000";
    expect(userHueIndex(pubkey, 12)).toBe(userHueIndex(pubkey, 12));
  });

  test("different pubkeys generally differ, and the index stays in range", () => {
    const keys = ["pk_a", "pk_b", "pk_c", "pk_d", "pk_e", "pk_f"];
    const hues = keys.map((k) => userHueIndex(k, 12));
    for (const hue of hues) {
      expect(hue).toBeGreaterThanOrEqual(0);
      expect(hue).toBeLessThan(12);
    }
    expect(new Set(hues).size).toBeGreaterThan(1);
  });
});
