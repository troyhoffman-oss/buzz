/**
 * Theme loading — DESIGN.md §3.10.
 *
 * The palettes are JSON under `themes/`, not TypeScript, for the reason stated
 * in {@link ../theme/tokens}: `just tui-check-boundary` fails on any hex
 * literal in `src/`, and that gate is deliberately blunt. Loading the palette
 * as data keeps the rule absolute rather than allowlisted.
 *
 * §3.10 also requires "pre-compute styles once at startup" — hence
 * {@link resolveTheme} being a one-shot resolution the app holds, not a lookup
 * performed per row.
 */

import latte from "../../themes/catppuccin-latte.json" with { type: "json" };
import macchiato from "../../themes/catppuccin-macchiato.json" with {
  type: "json",
};
import {
  type Palette,
  TOKEN_NAMES,
  type Theme,
  type ThemeMode,
  type TokenName,
  isColorless,
  modeFromEnv,
} from "./tokens";

/**
 * Validate a loaded palette against the closed token set.
 *
 * A palette missing a token would otherwise surface as `undefined` at a single
 * call site under one narrow condition — the worst shape of theme bug, because
 * it looks like a rendering glitch rather than a missing value.
 */
function toPalette(raw: Record<string, unknown>, source: string): Palette {
  const out: Record<string, string> = {};
  for (const name of TOKEN_NAMES) {
    const value = raw[name];
    if (typeof value !== "string") {
      throw new Error(`theme ${source}: token '${name}' is missing`);
    }
    out[name] = value;
  }
  return out as Palette;
}

/** The two palettes of the one theme, validated at module load. */
export const PALETTES: Readonly<Record<ThemeMode, Palette>> = {
  light: toPalette(latte as Record<string, unknown>, "catppuccin-latte"),
  dark: toPalette(macchiato as Record<string, unknown>, "catppuccin-macchiato"),
};

/** Resolve the active theme from the environment, once, at startup. */
export function resolveTheme(
  env: Record<string, string | undefined> = process.env,
): Theme {
  const mode = modeFromEnv(env);
  return { mode, palette: PALETTES[mode], colorless: isColorless(env) };
}

/** A theme with colour forced off, for `NO_COLOR` snapshots and T1 defaults. */
export function colorlessTheme(mode: ThemeMode = "dark"): Theme {
  return { mode, palette: PALETTES[mode], colorless: true };
}

export type { Palette, Theme, ThemeMode, TokenName };
