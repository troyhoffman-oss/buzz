import * as React from "react";
import { TerminalSquare } from "lucide-react";

import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";
import { cn } from "@/shared/lib/cn";
import { useTheme } from "@/shared/theme/ThemeProvider";
import { BuzzMark } from "@/shared/ui/buzz-logo/BuzzMark";
import chatgptLogoUrl from "../assets/harness-logos/chatgpt.png?inline";
import claudeLogoUrl from "../assets/harness-logos/claude.png?inline";
import gooseLogoUrl from "../assets/harness-logos/goose.png?inline";

// Bundled logos for compiled-in runtimes (inline base64, no network fetch).
const RUNTIME_LOGOS: Record<string, string> = {
  claude: claudeLogoUrl,
  codex: chatgptLogoUrl,
  goose: gooseLogoUrl,
};

// Public-path logos for bundled presets. Served from /harness-logos/ at runtime.
// Keys match the preset `id` values emitted by the backend PRESET_HARNESSES.
export const PRESET_LOGOS: Record<string, string> = {
  omp: "/harness-logos/omp.svg",
  grok: "/harness-logos/grok.svg",
  opencode: "/harness-logos/opencode.svg",
  kimi: "/harness-logos/kimi.png",
  amp: "/harness-logos/amp.png",
  hermes: "/harness-logos/hermes.png",
  openclaw: "/harness-logos/openclaw.svg",
};

function isBuzzRuntime(runtime: AcpRuntimeCatalogEntry): boolean {
  return runtime.id.trim().toLowerCase() === "buzz-agent";
}

export function getRuntimeDisplayLabel(
  runtime: AcpRuntimeCatalogEntry,
): string {
  return isBuzzRuntime(runtime) ? "Buzz" : runtime.label;
}

/**
 * The logo for a harness id, and the id that logo BELONGS to.
 *
 * A remote catalog advertises one entry per identity on the host — `hermes-matt`
 * beside `hermes` — and an exact-id lookup renders every one of them as the
 * generic TerminalSquare next to the plain entry's real mark. So a full id that
 * maps nothing falls back to its base: the text before the FIRST hyphen, and
 * only when that base is itself a mapped id, so `buzz-agent` (base `buzz`,
 * unmapped) is untouched and no id can be shortened into a logo it did not earn.
 *
 * The resolved id is returned alongside the url because the per-logo backdrop
 * classes below belong to the logo, not to the entry: a variant that borrows
 * `omp`'s white-on-black mark needs `omp`'s dark plate with it.
 *
 * Deliberately generic: nothing here knows what a Hermes profile is. Any
 * `<known>-<variant>` id gets the known harness's mark.
 */
function resolveHarnessLogo(
  harnessId: string,
): { id: string; url: string } | null {
  const id = harnessId.trim().toLowerCase();
  const exact = RUNTIME_LOGOS[id] ?? PRESET_LOGOS[id];
  if (exact) return { id, url: exact };
  const separator = id.indexOf("-");
  if (separator <= 0) return null;
  const base = id.slice(0, separator);
  const inherited = RUNTIME_LOGOS[base] ?? PRESET_LOGOS[base];
  return inherited ? { id: base, url: inherited } : null;
}

/** The logo url for a harness id. See `resolveHarnessLogo`. */
export function getHarnessLogoUrl(harnessId: string): string | null {
  return resolveHarnessLogo(harnessId)?.url ?? null;
}

export function RuntimeIcon({
  className = "h-8 w-8",
  runtime,
}: {
  className?: string;
  runtime: AcpRuntimeCatalogEntry;
}) {
  const [imageFailed, setImageFailed] = React.useState(false);
  const { isDark } = useTheme();
  // Only use bundled logo maps — never render user-supplied avatar URLs for
  // custom/preset entries (tracking pixel / spoofing vector, security line).
  const logo = resolveHarnessLogo(runtime.id);
  // The id the LOGO belongs to, so a variant entry gets its base's backdrop.
  // With no logo there is nothing to plate, and the id itself is what decides
  // the monochrome fallback treatment.
  const id = logo?.id ?? runtime.id.trim().toLowerCase();
  const imageUrl = logo?.url ?? null;
  const shouldForceForegroundColor = !imageUrl && id === "goose";

  if (isBuzzRuntime(runtime)) {
    return <BuzzMark className="h-7 w-10 text-foreground" />;
  }

  if (imageUrl && !imageFailed) {
    return (
      <img
        alt=""
        className={cn(
          "rounded-md object-contain",
          className,
          id === "omp" && "bg-[#0d0d0d] p-1",
          id === "grok" && "bg-white p-1",
          shouldForceForegroundColor &&
            (isDark ? "brightness-0 invert" : "brightness-0"),
        )}
        onError={() => setImageFailed(true)}
        src={imageUrl}
      />
    );
  }

  return (
    <TerminalSquare
      className={cn(className, "text-foreground")}
      strokeWidth={1.25}
    />
  );
}
