/**
 * The closed semantic token set — DESIGN.md §3.10 (unaffected by
 * NAVIGATION.md, per §7).
 *
 * Base theme is **Catppuccin**, Latte (light) and Macchiato (dark) as one theme
 * with two modes, matching the desktop and mobile apps.
 *
 * Two rules this module exists to make mechanical:
 *
 * 1. **No literal hex in feature code.** Feature modules import a token name;
 *    the hex lives here and only here. `just tui-check-boundary` fails the build
 *    on a `#rrggbb` anywhere in `src/`, so the palettes themselves are loaded
 *    from `themes/*.json` at startup rather than written inline — the gate is
 *    blunt on purpose and routing around it with an allowlist would make the
 *    next literal easy.
 * 2. **Neutral tones for large surfaces and chrome; high-chroma accents
 *    reserved for focus and selection.** §3.10 calls this "most of the
 *    difference between a TUI that looks designed and one that looks like a
 *    Christmas tree", and the token *names* are what encode it — `border` and
 *    `textMuted` are for chrome, `accent` is for the one selected row.
 */

/** Every token name in the closed set. Adding a colour means adding a name here. */
export const TOKEN_NAMES = [
  // semantic
  "primary",
  "secondary",
  "accent",
  "error",
  "warning",
  "success",
  "info",
  // text
  "text",
  "textMuted",
  "selectedListItemText",
  // surface
  "background",
  "backgroundPanel",
  "backgroundElement",
  "backgroundMenu",
  // border
  "border",
  "borderActive",
  "borderSubtle",
  // domain — diff
  "diffAdded",
  "diffRemoved",
  "diffContext",
  "diffHeader",
] as const;

/** A semantic token name. */
export type TokenName = (typeof TOKEN_NAMES)[number];

/** A resolved palette: every token mapped to a colour string. */
export type Palette = Readonly<Record<TokenName, string>>;

/** The two modes of the one theme (§3.10). */
export type ThemeMode = "light" | "dark";

/**
 * A resolved theme.
 *
 * `colorless` is not a third mode — it is the {@link NO_COLOR} / `TERM=dumb`
 * projection of whichever mode is active. §3.10 requires honouring both, and
 * modelling them as a *flag on a resolved theme* rather than as a palette of
 * empty strings keeps every call site's shape identical.
 */
export interface Theme {
  /** Which of the two modes this palette came from. */
  readonly mode: ThemeMode;
  /** Token → colour. */
  readonly palette: Palette;
  /** True when colour output is suppressed (`NO_COLOR`, `TERM=dumb`). */
  readonly colorless: boolean;
}

/**
 * Resolve a token to a renderable colour, or `undefined` when colour is off.
 *
 * Returning `undefined` rather than a default colour matters: OpenTUI treats an
 * absent `fg` as "inherit the terminal's own foreground", which is exactly what
 * `NO_COLOR` asks for. Substituting white would override a user's light
 * terminal with unreadable text.
 */
export function color(theme: Theme, token: TokenName): string | undefined {
  if (theme.colorless) return undefined;
  return theme.palette[token];
}

/**
 * Whether colour output should be suppressed, per §3.10.
 *
 * `NO_COLOR` follows the informal standard: *any* value, including the empty
 * string, disables colour — testing for truthiness would re-enable colour for
 * `NO_COLOR=`, which is the documented way to say "yes, disable it".
 */
export function isColorless(env: Record<string, string | undefined>): boolean {
  if (env.NO_COLOR !== undefined) return true;
  if (env.TERM === "dumb") return true;
  return false;
}

/**
 * Pick the mode from the environment.
 *
 * §3.10: "honouring the terminal's reported mode with an explicit lock
 * command". `BUZZ_TUI_THEME` is the lock; `COLORFGBG`'s second field is the
 * terminal's report (0–6 and 8 are dark backgrounds, by the de-facto
 * convention every terminal that emits it follows). Dark is the default because
 * an unreported terminal on a VPS is overwhelmingly a dark one, and because
 * light-on-light is less recoverable than dark-on-dark.
 */
export function modeFromEnv(
  env: Record<string, string | undefined>,
): ThemeMode {
  const locked = env.BUZZ_TUI_THEME;
  if (locked === "light" || locked === "dark") return locked;
  const fgbg = env.COLORFGBG;
  if (fgbg) {
    const background = fgbg.split(";").at(-1);
    if (background !== undefined && /^\d+$/.test(background)) {
      const n = Number(background);
      return n === 7 || n === 15 ? "light" : "dark";
    }
  }
  return "dark";
}

/**
 * Relative luminance per WCAG 2.x, from an `#rrggbb` string.
 *
 * Exported because §3.10 makes AA contrast over every (foreground, background)
 * pair **a CI test**, not a review note — so the check needs to live in code
 * that the test can import rather than in a spreadsheet.
 */
export function luminance(hex: string): number {
  const value = hex.replace("#", "");
  const full =
    value.length === 3
      ? value
          .split("")
          .map((c) => c + c)
          .join("")
      : value;
  const channel = (offset: number): number => {
    const srgb = Number.parseInt(full.slice(offset, offset + 2), 16) / 255;
    return srgb <= 0.03928 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4);
}

/** WCAG contrast ratio between two `#rrggbb` colours, 1..21. */
export function contrastRatio(a: string, b: string): number {
  const la = luminance(a);
  const lb = luminance(b);
  const [hi, lo] = la >= lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

/** WCAG AA for normal-size text. The §3.10 CI threshold. */
export const AA_NORMAL = 4.5;

/**
 * Deterministic per-user hue from a pubkey — §3.10.
 *
 * > Deterministic, so the same person is the same colour on both operators'
 * > boxes.
 *
 * FNV-1a over the hex string, folded to a hue index. The *index* is returned
 * rather than a colour so the lightness/chroma clamping stays in the palette
 * (where the theme mode is known) and no caller can produce an unclamped
 * colour.
 */
export function userHueIndex(pubkey: string, buckets: number): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < pubkey.length; i++) {
    hash ^= pubkey.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash % Math.max(1, buckets);
}
