import { AlertTriangle, Loader2 } from "lucide-react";
import * as React from "react";

import { useBackendProvidersQuery } from "@/features/agents/hooks";
import { useGlobalAgentConfig } from "@/features/agents/useGlobalAgentConfig";
import {
  discoverProviderHarnesses,
  probeBackendProvider,
  probeProviderModels,
} from "@/shared/api/tauri";
import type { RemoteHarness } from "@/shared/api/types";
import { Button } from "@/shared/ui/button";

import type { EnvVarsValue } from "./EnvVarsEditor";
import { PersonaDropdownField } from "./PersonaDropdownField";
import {
  coerceConfigValues,
  ProviderConfigFields,
} from "./ProviderConfigFields";
import {
  emptyWhereToRunDraft,
  LOCAL_RUN_TARGET_VALUE,
  providerConfigComplete,
  rememberProbedProviderName,
  runTargetOptions,
  type WhereToRunDraft,
} from "./whereToRunIntent";

/**
 * The create flow's first question: which computer this agent runs on, and —
 * once that answer is a backend provider — everything about that host.
 *
 * It leads the dialog because it is the one answer the rest of the form
 * depends on: the harness comes from the host's catalog and the models come
 * from the host's harness, so asking it last means answering the dependent
 * questions against the wrong machine first. Buzz shared compute is an LLM
 * provider, not a run destination, so it is not a choice here.
 */
export function WhereToRunSection({
  draft,
  envVars,
  isPending,
  onDraftChange,
}: {
  draft: WhereToRunDraft;
  /**
   * The definition's credential env, forwarded to the host's model probe. A
   * remote harness resolves its catalog from an API key exactly as the local
   * one does, so without these an Anthropic-backed harness would answer with
   * an auth error rather than a model list. Passed in (rather than read here)
   * because it is unsaved dialog state; the global layer beneath it is a
   * shared query, so this component reads that itself.
   */
  envVars: EnvVarsValue;
  isPending: boolean;
  onDraftChange: (next: WhereToRunDraft) => void;
}) {
  const backendProviders = useBackendProvidersQuery().data ?? [];
  const { globalConfig } = useGlobalAgentConfig();
  const [probeError, setProbeError] = React.useState<string | null>(null);
  const [harnessError, setHarnessError] = React.useState<string | null>(null);
  const [isDiscoveringHarnesses, setIsDiscoveringHarnesses] =
    React.useState(false);
  // Friendly provider names, accumulated as probes land. Kept here rather than
  // derived from the current selection so a provider keeps one name for the
  // life of the dialog instead of renaming itself as the cursor moves.
  const [probedProviderNames, setProbedProviderNames] = React.useState<
    Readonly<Record<string, string>>
  >({});
  const isProviderMode = draft.runOn !== LOCAL_RUN_TARGET_VALUE;
  const selectedBackendProvider = React.useMemo(
    () =>
      backendProviders.find((provider) => provider.id === draft.runOn) ?? null,
    [backendProviders, draft.runOn],
  );

  // The probe effect writes back into the draft it reads. Reading it through a
  // ref instead of the dependency array is what keeps that from being a
  // self-retriggering loop (probe → onDraftChange → new draft identity →
  // probe): the provider selection is the only thing that should re-probe.
  const draftRef = React.useRef(draft);
  draftRef.current = draft;
  // Read at call time for the same reason the harness catalog is: an env edit
  // must not open an SSH connection per keystroke.
  //
  // Global sits UNDER the definition's env, the same order `provider_deploy`
  // merges on the host. Without the global layer a key satisfied globally —
  // which the dialog then shows as inherited, with no required marker, so the
  // user has no reason to restate it — never reaches the probe, and the host
  // answers the model request with an auth error for a credential that is in
  // fact configured.
  const probeEnvRef = React.useRef<EnvVarsValue>({});
  probeEnvRef.current = { ...globalConfig.env_vars, ...envVars };
  // Serial number of the newest host request. Every catalog read and model
  // probe claims one at its start and re-checks it after each await; anything
  // that moves the draft off the host/harness a request was made for bumps it,
  // so the stale continuation drops its answer instead of writing it back.
  const hostRequestRef = React.useRef(0);

  React.useEffect(() => {
    if (!isProviderMode || !selectedBackendProvider) {
      setProbeError(null);
      return;
    }
    let cancelled = false;
    setProbeError(null);
    void probeBackendProvider(selectedBackendProvider.binaryPath)
      .then((result) => {
        if (cancelled) return;
        setProbedProviderNames((previous) =>
          rememberProbedProviderName(
            previous,
            selectedBackendProvider.id,
            result,
          ),
        );
        const defaults: Record<string, string> = {};
        const properties =
          (result.config_schema as Record<string, unknown> | undefined)
            ?.properties ?? {};
        for (const [key, property] of Object.entries(properties) as [
          string,
          Record<string, unknown>,
        ][]) {
          if (property.default != null)
            defaults[key] = String(property.default);
        }
        onDraftChange({
          ...draftRef.current,
          probedProvider: result,
          // Schema defaults are seeded UNDERNEATH what the user has typed.
          // The probe is a round-trip to the provider binary and re-runs
          // whenever it resolves anew, so overwriting here would wipe an
          // address mid-edit. A provider switch empties the draft, so on the
          // first probe of a provider this is exactly `defaults`.
          providerConfig: { ...defaults, ...draftRef.current.providerConfig },
        });
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setProbeError(error instanceof Error ? error.message : String(error));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [isProviderMode, onDraftChange, selectedBackendProvider]);

  /**
   * Abandon every in-flight host request. Their answers describe a
   * host/harness the draft no longer targets, so landing one would scope the
   * picker to the wrong machine — or, for a model probe, ship the definition's
   * credentials to a host under a harness command never verified there.
   */
  function discardHostRequests() {
    hostRequestRef.current += 1;
  }

  /** Claim the newest request id, invalidating anything already in flight. */
  function startHostRequest(): number {
    discardHostRequests();
    return hostRequestRef.current;
  }

  // Discovery is an explicit action, not an effect on the config fields: it
  // opens a real SSH connection to the host, which must not happen once per
  // keystroke while the address is being typed.
  async function handleDiscoverHarnesses() {
    if (!selectedBackendProvider) return;
    const requestId = startHostRequest();
    setHarnessError(null);
    setIsDiscoveringHarnesses(true);
    try {
      const config = coerceConfigValues(
        draftRef.current.providerConfig,
        draftRef.current.probedProvider?.config_schema,
      );
      const catalog = await discoverProviderHarnesses(
        selectedBackendProvider.binaryPath,
        config,
      );
      // The config can be edited (or the provider re-picked) while this read
      // is open. That answer then describes an abandoned host: re-installing
      // its catalog would resurrect a pin the edit deliberately cleared, and
      // the probe below would send credentials to the NEW host under the OLD
      // host's harness command. Drop it.
      if (hostRequestRef.current !== requestId) return;
      // Keep an existing pick when a re-check still offers it; otherwise fall
      // to the first available so the common case needs no extra interaction.
      const previous = draftRef.current.remoteHarnessId;
      const keep = catalog.harnesses.find(
        (harness) => harness.available && harness.id === previous,
      );
      const firstAvailable =
        keep ?? catalog.harnesses.find((harness) => harness.available) ?? null;
      const next = {
        ...draftRef.current,
        remoteHarnesses: catalog.harnesses,
        remoteHarnessId: firstAvailable?.id ?? null,
        remoteModelProbe: { status: "idle" } as const,
      };
      onDraftChange(next);
      if (!catalog.buzzAcp) {
        // Deploy installs buzz-acp only when this desktop has a binary to
        // push (see docs/remote-agents.md); without one it fails with install
        // guidance. The copy promises the union honestly rather than guessing
        // which case applies from here.
        setHarnessError(
          "buzz-acp is not installed on that host. Deploy will install it or explain how to.",
        );
      }
      // A re-check can change what the auto-picked harness resolves to even
      // when the id is unchanged (a reinstall, a different PATH entry), so the
      // catalog read always re-probes rather than trusting a prior result.
      if (firstAvailable) void probeModels(firstAvailable, next);
    } catch (error: unknown) {
      if (hostRequestRef.current !== requestId) return;
      setHarnessError(error instanceof Error ? error.message : String(error));
    } finally {
      // Unconditional: the button is disabled while this flag is set, so no
      // second catalog read can be in flight to own it — and the model probe
      // started just above deliberately claims a newer id. Guarding here would
      // strand "Checking host…" on screen with no way to retry.
      setIsDiscoveringHarnesses(false);
    }
  }

  /**
   * Read the picked harness's model catalog FROM THE HOST.
   *
   * This is the whole point of the remote path: `get_agent_models` /
   * `discover_agent_models` answer for this computer, so a model chosen from
   * their list is validated against the wrong machine — the exact
   * remote/local confusion a provider-backed create exists to avoid.
   *
   * Failure is non-fatal by design. The host's catalog scopes the picker; it
   * does not gate the create, and the harness's own default remains a valid
   * choice when the probe cannot run.
   */
  async function probeModels(harness: RemoteHarness, base: WhereToRunDraft) {
    if (!selectedBackendProvider) return;
    const requestId = startHostRequest();
    // `base` rather than `draftRef.current`: the caller has just published the
    // harness pick, and React has not re-rendered yet, so the ref still holds
    // the pre-pick draft. Spreading it here would revert the pick.
    onDraftChange({ ...base, remoteModelProbe: { status: "loading" } });
    try {
      const models = await probeProviderModels(
        selectedBackendProvider.binaryPath,
        coerceConfigValues(
          base.providerConfig,
          base.probedProvider?.config_schema,
        ),
        harness,
        // The harness's own catalog env rides underneath the user's, so a
        // user-set key wins over a default exactly as it does at spawn.
        { ...harness.env, ...probeEnvRef.current },
      );
      if (hostRequestRef.current !== requestId) return;
      onDraftChange({
        ...draftRef.current,
        remoteModelProbe: { status: "loaded", models },
      });
    } catch (error: unknown) {
      if (hostRequestRef.current !== requestId) return;
      onDraftChange({
        ...draftRef.current,
        remoteModelProbe: {
          status: "failed",
          error: error instanceof Error ? error.message : String(error),
        },
      });
    }
  }

  function handleSelectHarness(remoteHarnessId: string) {
    discardHostRequests();
    const harness = (draft.remoteHarnesses ?? []).find(
      (candidate) => candidate.id === remoteHarnessId,
    );
    const next = {
      ...draft,
      remoteHarnessId,
      remoteModelProbe: { status: "idle" } as const,
    };
    if (!harness) {
      onDraftChange(next);
      return;
    }
    void probeModels(harness, next);
  }

  // No provider installed is a legitimate answer to this question, not a
  // reason to skip it. As the LAST step the section could vanish silently; as
  // the FIRST one, vanishing would leave the user with no evidence that
  // running elsewhere is even a thing Buzz does — so the control stays and
  // explains why it has only one entry.
  const hasProviders = backendProviders.length > 0;

  return (
    <div className="space-y-4">
      <div className="space-y-1.5">
        <label
          className="text-sm font-medium text-foreground"
          htmlFor="agent-run-on"
        >
          Where does this agent run?
        </label>
        <PersonaDropdownField
          disabled={isPending || !hasProviders}
          id="agent-run-on"
          onValueChange={(runOn) => {
            discardHostRequests();
            onDraftChange({ ...emptyWhereToRunDraft, runOn });
          }}
          options={runTargetOptions(backendProviders, probedProviderNames)}
          placeholder="This computer"
          value={draft.runOn}
        />
        {!hasProviders ? (
          <p className="text-xs text-muted-foreground">
            Install a backend provider to run agents on another machine.
          </p>
        ) : null}
      </div>

      {isProviderMode && selectedBackendProvider ? (
        <div className="space-y-4">
          <div className="flex gap-3 rounded-2xl border border-warning/30 bg-warning-bg px-4 py-3">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
            <p className="text-sm text-warning">
              This provider at{" "}
              <span className="font-mono font-medium">
                {selectedBackendProvider.binaryPath}
              </span>{" "}
              will receive your agent&apos;s private key. Only use providers
              from trusted sources.
            </p>
          </div>
          {probeError ? (
            <p className="rounded-2xl border border-destructive/30 bg-destructive/10 px-4 py-3 text-sm text-destructive">
              Could not probe provider: {probeError}
            </p>
          ) : null}
          {draft.probedProvider?.config_schema ? (
            <ProviderConfigFields
              config={draft.providerConfig}
              onChange={(providerConfig) => {
                discardHostRequests();
                onDraftChange({
                  ...draft,
                  providerConfig,
                  // Config edits invalidate a catalog read from the previous
                  // host — a stale pin would deploy a command that may not
                  // exist on the new one, and stale models would scope the
                  // picker to a machine the agent is no longer going to.
                  remoteHarnesses: null,
                  remoteHarnessId: null,
                  remoteModelProbe: { status: "idle" },
                });
              }}
              schema={draft.probedProvider.config_schema}
            />
          ) : null}

          <RemoteHarnessPicker
            draft={draft}
            error={harnessError}
            isDiscovering={isDiscoveringHarnesses}
            isPending={isPending}
            onDiscover={() => void handleDiscoverHarnesses()}
            onSelect={handleSelectHarness}
          />
        </div>
      ) : null}
    </div>
  );
}

/**
 * Harness selection for a remote agent. Deliberately separate from the local
 * runtime picker: that one lists what is installed on THIS computer, which says
 * nothing about the host, and the entry chosen here is what the deploy actually
 * runs there.
 */
function RemoteHarnessPicker({
  draft,
  error,
  isDiscovering,
  isPending,
  onDiscover,
  onSelect,
}: {
  draft: WhereToRunDraft;
  error: string | null;
  isDiscovering: boolean;
  isPending: boolean;
  onDiscover: () => void;
  onSelect: (harnessId: string) => void;
}) {
  const canDiscover = providerConfigComplete(draft) && !isPending;
  const harnesses = draft.remoteHarnesses;
  const available = (harnesses ?? []).filter((harness) => harness.available);

  return (
    <div className="space-y-1.5">
      <label className="text-sm font-medium" htmlFor="agent-remote-harness">
        Harness on the host
      </label>
      {harnesses === null ? (
        <p className="text-sm text-muted-foreground">
          Agents run the harness installed on the host, not on this computer.
        </p>
      ) : available.length === 0 ? (
        <p className="rounded-2xl border border-destructive/30 bg-destructive/10 px-4 py-3 text-sm text-destructive">
          No supported harness is installed on that host. Install one (for
          example <span className="font-mono">goose</span>) and check again.
        </p>
      ) : (
        <PersonaDropdownField
          disabled={isPending}
          id="agent-remote-harness"
          onValueChange={onSelect}
          options={available.map((harness) => ({
            label: `${harness.label}${
              harness.version ? ` (${harness.version})` : ""
            }`,
            value: harness.id,
          }))}
          placeholder="Select a harness"
          value={draft.remoteHarnessId ?? ""}
        />
      )}
      {error ? <p className="text-sm text-warning">{error}</p> : null}
      {/* Always available, in every state: a failed connection, an empty
          catalog and a just-installed harness all need a second attempt, and
          hiding the button after the first one strands the user in the create
          dialog with no path forward. */}
      <Button
        disabled={!canDiscover || isDiscovering}
        onClick={onDiscover}
        size="sm"
        type="button"
        variant="outline"
      >
        {isDiscovering ? (
          <Loader2 className="mr-2 h-4 w-4 animate-spin" />
        ) : null}
        {isDiscovering
          ? "Checking host…"
          : harnesses === null
            ? "Check the host"
            : "Check again"}
      </Button>
    </div>
  );
}
