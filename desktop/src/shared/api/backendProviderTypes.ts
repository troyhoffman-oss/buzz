/**
 * Types for backend providers: the `buzz-backend-*` binaries that run an agent
 * somewhere other than this computer.
 *
 * Kept out of `types.ts` because everything here describes a REMOTE machine,
 * and conflating it with the local-runtime vocabulary is precisely the mistake
 * that makes a remote agent silently deploy the wrong harness.
 */

export type BackendProviderCandidate = {
  id: string;
  binaryPath: string;
};

export type BackendProviderProbeResult = {
  ok: boolean;
  name?: string;
  version?: string;
  description?: string;
  config_schema?: Record<string, unknown>;
};

/**
 * One harness on the machine a provider deploys to, from the provider's
 * `discover_harnesses` op. `command`/`args`/`env` describe the REMOTE host, so
 * they are pinned onto the agent record verbatim at create time — nothing
 * re-resolves them, because a provider-backed agent never spawns locally.
 */
export type RemoteHarness = {
  id: string;
  label: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  available: boolean;
  binaryPath: string | null;
  version: string | null;
};

export type RemoteHarnessCatalog = {
  /** `null` when buzz-acp is not installed on the host — actionable, not fatal. */
  buzzAcp: { path: string; version: string } | null;
  harnesses: RemoteHarness[];
};
