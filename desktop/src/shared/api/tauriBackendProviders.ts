/**
 * The Tauri surface for backend providers: the `buzz-backend-*` binaries that
 * run an agent somewhere other than this computer.
 *
 * Split out of `tauri.ts` for the same reason `backendProviderTypes.ts` is
 * split out of `types.ts` — every call here answers a question about a REMOTE
 * machine, and the local-runtime bindings next door answer it about this one.
 * Keeping the two apart is what stops a remote agent silently probing the
 * wrong host's harness catalog.
 */

import type {
  AgentModelsResponse,
  BackendProviderCandidate,
  BackendProviderProbeResult,
  RemoteHarness,
  RemoteHarnessCatalog,
} from "@/shared/api/types";
import { invokeTauri } from "@/shared/api/tauri";

export async function discoverBackendProviders(): Promise<
  BackendProviderCandidate[]
> {
  return invokeTauri<BackendProviderCandidate[]>("discover_backend_providers");
}

export async function probeBackendProvider(
  binaryPath: string,
): Promise<BackendProviderProbeResult> {
  return invokeTauri<BackendProviderProbeResult>("probe_backend_provider", {
    binaryPath,
  });
}

type RawRemoteHarness = {
  id: string;
  label?: string | null;
  command: string;
  args?: string[] | null;
  env?: Record<string, string> | null;
  available?: boolean | null;
  binaryPath?: string | null;
  version?: string | null;
  exclusive?: boolean | null;
};

type RawRemoteHarnessCatalog = {
  buzz_acp?: { path: string; version: string } | null;
  harnesses?: RawRemoteHarness[] | null;
};

/**
 * One catalog row, wire → app.
 *
 * Exported for the same reason `fromRawAcpRuntimeCatalogEntry` is: the mapping
 * is the API boundary contract, and a test that re-implements it proves
 * nothing.
 */
export function fromRawRemoteHarness(harness: RawRemoteHarness): RemoteHarness {
  return {
    id: harness.id,
    label: harness.label ?? harness.id,
    command: harness.command,
    args: harness.args ?? [],
    env: harness.env ?? {},
    available: harness.available ?? false,
    binaryPath: harness.binaryPath ?? null,
    version: harness.version ?? null,
    // Only carried when the provider asserted it. Spreading a `false` for
    // every other entry would put the desktop in the business of claiming
    // something the provider never said; absent IS the default, and every
    // consumer reads it as "no limit".
    ...(harness.exclusive === true ? { exclusive: true } : {}),
  };
}

/**
 * The harness catalog of the machine the provider deploys to.
 *
 * The local ACP runtime catalog describes THIS computer, which for a remote
 * agent is the wrong machine entirely — this is what the create dialog reads
 * instead, and the picked entry's `command` becomes the create-time harness
 * pin that the deploy ships to the host.
 */
export async function discoverProviderHarnesses(
  binaryPath: string,
  config: Record<string, unknown>,
): Promise<RemoteHarnessCatalog> {
  const raw = await invokeTauri<RawRemoteHarnessCatalog>(
    "discover_provider_harnesses",
    { binaryPath, config },
  );
  return {
    buzzAcp: raw.buzz_acp ?? null,
    harnesses: (raw.harnesses ?? []).map(fromRawRemoteHarness),
  };
}

/**
 * Model catalog for one remote harness. Normalized backend-side through the
 * same `normalize_agent_models` the local path uses, so the model picker needs
 * no remote-specific rendering.
 */
export async function probeProviderModels(
  binaryPath: string,
  config: Record<string, unknown>,
  harness: Pick<RemoteHarness, "command" | "args">,
  envVars?: Record<string, string>,
): Promise<AgentModelsResponse> {
  return invokeTauri<AgentModelsResponse>("probe_provider_models", {
    binaryPath,
    config,
    harness: { command: harness.command, args: harness.args },
    envVars,
  });
}
