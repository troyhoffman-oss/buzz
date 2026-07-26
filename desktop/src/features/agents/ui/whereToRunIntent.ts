import type { BackendIntent } from "../lib/instanceInputForDefinition";
import type {
  AgentModelsResponse,
  BackendProviderProbeResult,
  RemoteHarness,
} from "@/shared/api/types";
import type { PersonaModelOption } from "./agentConfigOptions";
import type { PersonaModelDiscoveryStatus } from "./personaModelDiscoveryStatus";
import { coerceConfigValues } from "./ProviderConfigFields";
import { getDiscoveredPersonaModelOptions } from "./usePersonaModelDiscovery";

/**
 * The model catalog of the picked remote harness, read from the HOST by
 * `probe_provider_models`.
 *
 * A provider-backed agent runs its harness on the host, so its models are the
 * host's models. The local discovery path would answer with this computer's
 * catalog — a different machine, and for a remote-only harness usually an
 * empty or failed one — which is why a remote create reads this instead.
 */
export type RemoteModelProbe =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "loaded"; models: AgentModelsResponse }
  | { status: "failed"; error: string };

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
  /** Models of the picked entry, probed on the host. */
  remoteModelProbe: RemoteModelProbe;
};

export const emptyWhereToRunDraft: WhereToRunDraft = {
  runOn: "local",
  providerConfig: {},
  probedProvider: null,
  remoteHarnesses: null,
  remoteHarnessId: null,
  remoteModelProbe: { status: "idle" },
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
 * What the create dialog's Model control renders for a provider-backed create.
 *
 * Deliberately the same shape `usePersonaModelDiscovery` returns, so the
 * dialog swaps one for the other rather than growing a parallel remote
 * rendering path. `harnessId` is the reset key: changing the harness resets
 * the dependent model exactly as changing the local one does.
 */
export type RemoteModelDiscoveryView = {
  harnessId: string;
  discoveredModelOptions: readonly PersonaModelOption[] | null;
  modelDiscoveryLoading: boolean;
  modelDiscoveryStatus: PersonaModelDiscoveryStatus | null;
};

/**
 * Project the host's model probe into the dialog's Model control.
 *
 * `null` means "the local path owns this control": either the agent runs
 * locally, or no remote harness has been picked yet so there is nothing to
 * have probed.
 *
 * The status copy is remote-specific on purpose. The local failure copy
 * ("using built-in model options") is a lie here — there is no built-in
 * catalog for someone else's machine, and the actionable step is on the host,
 * not in this dialog.
 */
export function remoteModelDiscoveryView(
  draft: WhereToRunDraft,
): RemoteModelDiscoveryView | null {
  const harness = selectedRemoteHarness(draft);
  if (!harness) return null;
  const probe = draft.remoteModelProbe;
  if (probe.status === "idle") return null;

  const base = {
    harnessId: harness.id,
    modelDiscoveryLoading: probe.status === "loading",
  };
  if (probe.status === "loading") {
    return {
      ...base,
      discoveredModelOptions: null,
      modelDiscoveryStatus: null,
    };
  }
  if (probe.status === "failed") {
    return {
      ...base,
      discoveredModelOptions: null,
      modelDiscoveryStatus: {
        message: `Could not load models from the host: ${probe.error}`,
        tone: "warning",
      },
    };
  }

  // Provider is fixed as "" rather than the definition's: that argument only
  // decides whether a "Default model" row is offered, and for a remote harness
  // the host's own default is always a legitimate choice.
  const options = getDiscoveredPersonaModelOptions(probe.models, "");
  return {
    ...base,
    discoveredModelOptions: options,
    modelDiscoveryStatus:
      options === null
        ? {
            message: `${
              probe.models.agentName.trim() || "That harness"
            } reported no models on the host. Check that it is installed and signed in there, then check the host again.`,
            tone: "warning",
          }
        : null,
  };
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
