import type { BackendIntent } from "../lib/instanceInputForDefinition";
import type {
  BackendProviderProbeResult,
  RemoteHarness,
} from "@/shared/api/types";
import { coerceConfigValues } from "./ProviderConfigFields";

/** Draft state of the optional remote-backend selector. */
export type WhereToRunDraft = {
  runOn: "local" | string;
  providerConfig: Record<string, string>;
  probedProvider: BackendProviderProbeResult | null;
  /**
   * The harness catalog of the REMOTE host, once `discover_provider_harnesses`
   * has run against the entered config. `null` means "not discovered yet".
   */
  remoteHarnesses: readonly RemoteHarness[] | null;
  /** Id of the picked entry of `remoteHarnesses`. */
  remoteHarnessId: string | null;
};

export const emptyWhereToRunDraft: WhereToRunDraft = {
  runOn: "local",
  providerConfig: {},
  probedProvider: null,
  remoteHarnesses: null,
  remoteHarnessId: null,
};

export function providerConfigComplete(draft: WhereToRunDraft): boolean {
  if (draft.runOn === "local") return true;
  if (!draft.probedProvider) return false;
  const schema = draft.probedProvider.config_schema as
    | Record<string, unknown>
    | undefined;
  const required: string[] = (schema?.required as string[] | undefined) ?? [];
  return required.every(
    (key) => (draft.providerConfig[key] ?? "").trim().length > 0,
  );
}

/** The picked remote harness, or null when none is selected/available. */
export function selectedRemoteHarness(
  draft: WhereToRunDraft,
): RemoteHarness | null {
  if (draft.runOn === "local" || !draft.remoteHarnessId) return null;
  return (
    draft.remoteHarnesses?.find(
      (harness) => harness.id === draft.remoteHarnessId,
    ) ?? null
  );
}

/**
 * A provider create must carry a harness from the remote catalog: it is the
 * only channel by which the harness choice reaches the host, and without it the
 * record would fall back to the locally-resolved default (`buzz-agent`) and the
 * host would silently provision a harness the user never chose. So the submit
 * button stays blocked until one is picked, rather than letting the create fail
 * later inside `buildInstanceInputForDefinition`.
 */
export function canSubmitWhereToRun(draft: WhereToRunDraft): boolean {
  if (!providerConfigComplete(draft)) return false;
  if (draft.runOn === "local") return true;
  return selectedRemoteHarness(draft) !== null;
}

export function resolveBackendIntent(
  draft: WhereToRunDraft,
): BackendIntent | null {
  if (draft.runOn === "local") return null;
  const harness = selectedRemoteHarness(draft);
  return {
    type: "provider",
    id: draft.runOn,
    config: coerceConfigValues(
      draft.providerConfig,
      draft.probedProvider?.config_schema,
    ),
    ...(harness
      ? {
          harness: {
            id: harness.id,
            command: harness.command,
            args: harness.args,
            env: harness.env,
          },
        }
      : {}),
  };
}
