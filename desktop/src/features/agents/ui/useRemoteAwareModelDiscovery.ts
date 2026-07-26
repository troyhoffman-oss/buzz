import * as React from "react";

import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";
import {
  type PersonaModelOption,
  runtimeSupportsLlmProviderSelection,
} from "./agentConfigOptions";
import type { EnvVarsValue } from "./EnvVarsEditor";
import type { PersonaModelDiscoveryStatus } from "./personaModelDiscoveryStatus";
import { usePersonaModelDiscovery } from "./usePersonaModelDiscovery";
import type { RemoteModelDiscoveryView } from "./whereToRunIntent";

/** What the dialog's Model control needs, whichever machine answered. */
export type ModelDiscoveryView = {
  discoveredModelOptions: readonly PersonaModelOption[] | null;
  modelDiscoveryLoading: boolean;
  modelDiscoveryStatus: PersonaModelDiscoveryStatus | null;
};

type LocalModelDiscoveryInput = {
  /** The definition's credential env, and the global layer beneath it. */
  envVars: EnvVarsValue;
  globalEnvVars: EnvVarsValue;
  isCustomProviderEditing: boolean;
  modelFieldVisible: boolean;
  open: boolean;
  /** The effective provider (agent → global → file chain). */
  provider: string;
  runtime: string;
  selectedRuntime: AcpRuntimeCatalogEntry | undefined;
};

/**
 * Resolve the model catalog of the machine the agent will actually run on.
 *
 * The two catalogs are never merged: they come from different computers, so
 * their union would offer models the chosen harness cannot run. Once the host
 * has answered, its catalog wins outright — and local discovery is suppressed
 * entirely, since running a CLI here to describe a machine the agent will
 * never run on is pure noise (and needless credential use).
 *
 * Switching harnesses invalidates the selected model for the same reason
 * switching the local runtime does — the id came from the previous harness's
 * catalog and generally means nothing to the next one — so `onHarnessChange`
 * fires on every change of the remote harness, remote → local included.
 */
export function useRemoteAwareModelDiscovery({
  local,
  remote,
  onHarnessChange,
}: {
  local: LocalModelDiscoveryInput;
  remote: RemoteModelDiscoveryView | null;
  onHarnessChange: () => void;
}): ModelDiscoveryView {
  // Global env is the base layer so credential keys satisfied globally still
  // reach discovery — same rationale as in AgentInstanceEditDialog.
  const envVars = React.useMemo(
    () => ({ ...local.globalEnvVars, ...local.envVars }),
    [local.globalEnvVars, local.envVars],
  );
  const localDiscovery = usePersonaModelDiscovery({
    envVars,
    isCustomProviderEditing: local.isCustomProviderEditing,
    modelFieldVisible: local.modelFieldVisible && remote === null,
    open: local.open,
    // Gate provider by runtime: runtimes that don't choose their own LLM
    // provider (codex, claude) must not inherit the global one — doing so
    // discovers models from the wrong provider.
    provider: runtimeSupportsLlmProviderSelection(local.runtime)
      ? local.provider
      : "",
    selectedRuntime: local.selectedRuntime,
  });

  const harnessId = remote?.harnessId ?? null;
  const lastHarnessIdRef = React.useRef(harnessId);
  // Held in a ref so an inline callback does not re-run the effect every
  // render and clear a model the user just picked.
  const onHarnessChangeRef = React.useRef(onHarnessChange);
  onHarnessChangeRef.current = onHarnessChange;

  React.useEffect(() => {
    if (lastHarnessIdRef.current === harnessId) return;
    lastHarnessIdRef.current = harnessId;
    onHarnessChangeRef.current();
  }, [harnessId]);

  return remote ?? localDiscovery;
}
