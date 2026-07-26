/**
 * Pure logic for the Remote servers gallery (RemoteServersCard).
 *
 * Extracted for deterministic unit-testing — no React, no Tauri, no network —
 * exactly as `harnessGalleryLogic` is for the harness gallery it mirrors.
 */

import { backendProviderLabel } from "@/features/agents/lib/backendProviderLabel";
import type {
  BackendProviderCandidate,
  BackendProviderProbeResult,
} from "@/shared/api/types";

/** What a provider's `info` probe has told us so far. */
export type RemoteServerProbe =
  | { status: "loading" }
  | { status: "ok"; result: BackendProviderProbeResult }
  | { status: "failed"; error: string };

/** One row of the gallery. */
export type RemoteServerEntry = {
  id: string;
  binaryPath: string;
  /** The provider's own `info.name` once probed, else its binary-derived id. */
  label: string;
  /** `info.version`, or `null` while probing / when the provider omits it. */
  version: string | null;
  /** `info.description`, or `null`. */
  description: string | null;
  /**
   * `"probing"` while `info` is in flight, `"ready"` once it answered `ok`,
   * `"unavailable"` when the probe failed or the provider answered `ok: false`.
   *
   * `"ready"` means "this binary answers the provider protocol", NOT "the
   * server is reachable". `info` is the one op that opens no connection (see
   * docs/remote-agents.md) — reachability is a per-host question, and the host
   * is chosen per-agent in the create dialog, so this surface cannot answer it.
   */
  status: "probing" | "ready" | "unavailable";
  /** The probe's failure message, for an `"unavailable"` row. */
  error: string | null;
};

function probeError(probe: RemoteServerProbe | undefined): string | null {
  if (probe?.status === "failed") return probe.error;
  if (probe?.status === "ok" && !probe.result.ok) {
    return "The provider did not answer its info request.";
  }
  return null;
}

/**
 * Project discovered providers plus their probes into gallery rows.
 *
 * Ready-first then alphabetical, mirroring `sortedPresetEntries`: discovery
 * walks `PATH`, so leaving rows in discovery order would let the gallery
 * reshuffle itself between reads.
 */
export function remoteServerEntries(
  providers: readonly BackendProviderCandidate[],
  probes: Readonly<Record<string, RemoteServerProbe>>,
): RemoteServerEntry[] {
  const entries = providers.map((provider): RemoteServerEntry => {
    const probe = probes[provider.id];
    const info = probe?.status === "ok" ? probe.result : undefined;
    const error = probeError(probe);
    return {
      id: provider.id,
      binaryPath: provider.binaryPath,
      label: backendProviderLabel(provider.id, info?.ok ? info.name : null),
      version: (info?.ok && info.version?.trim()) || null,
      description: (info?.ok && info.description?.trim()) || null,
      status: error ? "unavailable" : info?.ok ? "ready" : "probing",
      error,
    };
  });

  return entries.sort((left, right) => {
    const leftReady = left.status === "ready" ? 0 : 1;
    const rightReady = right.status === "ready" ? 0 : 1;
    if (leftReady !== rightReady) return leftReady - rightReady;
    return left.label.localeCompare(right.label);
  });
}

/** `"SSH 0.4.26"` — the label with its version, when there is one. */
export function remoteServerVersionLabel(entry: RemoteServerEntry): string {
  return entry.version ? `${entry.label} ${entry.version}` : entry.label;
}
