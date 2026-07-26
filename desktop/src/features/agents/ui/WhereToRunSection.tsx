import { AlertTriangle, Loader2 } from "lucide-react";
import * as React from "react";

import { useBackendProvidersQuery } from "@/features/agents/hooks";
import {
  discoverProviderHarnesses,
  probeBackendProvider,
  probeProviderModels,
} from "@/shared/api/tauri";
import type { RemoteHarness } from "@/shared/api/types";
import { Button } from "@/shared/ui/button";

import type { EnvVarsValue } from "./EnvVarsEditor";
import {
  coerceConfigValues,
  ProviderConfigFields,
} from "./ProviderConfigFields";
import {
  emptyWhereToRunDraft,
  providerConfigComplete,
  type WhereToRunDraft,
} from "./whereToRunIntent";

/** Optional remote-backend selector. Buzz shared compute is an LLM provider, not a run destination. */
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
   * an auth error rather than a model list.
   */
  envVars: EnvVarsValue;
  isPending: boolean;
  onDraftChange: (next: WhereToRunDraft) => void;
}) {
  const backendProviders = useBackendProvidersQuery().data ?? [];
  const [probeError, setProbeError] = React.useState<string | null>(null);
  const [harnessError, setHarnessError] = React.useState<string | null>(null);
  const [isDiscoveringHarnesses, setIsDiscoveringHarnesses] =
    React.useState(false);
  const isProviderMode = draft.runOn !== "local";
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
  const envVarsRef = React.useRef(envVars);
  envVarsRef.current = envVars;
  // Discards a model probe that resolves after a newer one was started (the
  // user re-picked, or re-checked the host, while the first was in flight).
  const modelProbeRequestRef = React.useRef(0);

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
          providerConfig: defaults,
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
   * Abandon any in-flight model probe. Its answer describes a host/harness the
   * draft no longer targets, so landing it would scope the picker to the wrong
   * machine.
   */
  function discardModelProbe() {
    modelProbeRequestRef.current += 1;
  }

  // Discovery is an explicit action, not an effect on the config fields: it
  // opens a real SSH connection to the host, which must not happen once per
  // keystroke while the address is being typed.
  async function handleDiscoverHarnesses() {
    if (!selectedBackendProvider) return;
    discardModelProbe();
    setHarnessError(null);
    setIsDiscoveringHarnesses(true);
    try {
      const catalog = await discoverProviderHarnesses(
        selectedBackendProvider.binaryPath,
        coerceConfigValues(
          draftRef.current.providerConfig,
          draftRef.current.probedProvider?.config_schema,
        ),
      );
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
        setHarnessError(
          "buzz-acp is not installed on that host. The deploy will install it.",
        );
      }
      // A re-check can change what the auto-picked harness resolves to even
      // when the id is unchanged (a reinstall, a different PATH entry), so the
      // catalog read always re-probes rather than trusting a prior result.
      if (firstAvailable) void probeModels(firstAvailable, next);
    } catch (error: unknown) {
      setHarnessError(error instanceof Error ? error.message : String(error));
    } finally {
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
    const requestId = modelProbeRequestRef.current + 1;
    modelProbeRequestRef.current = requestId;
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
        // The harness's own catalog env rides underneath the definition's, so
        // a user-set key wins over a default exactly as it does at spawn.
        { ...harness.env, ...envVarsRef.current },
      );
      if (modelProbeRequestRef.current !== requestId) return;
      onDraftChange({
        ...draftRef.current,
        remoteModelProbe: { status: "loaded", models },
      });
    } catch (error: unknown) {
      if (modelProbeRequestRef.current !== requestId) return;
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
    discardModelProbe();
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

  if (backendProviders.length === 0) return null;

  return (
    <div className="space-y-4">
      <div className="space-y-1.5">
        <label className="text-sm font-medium" htmlFor="agent-run-on">
          Run on
        </label>
        <select
          className="flex h-9 w-full rounded-md border border-input bg-background px-3 py-2 text-sm shadow-xs"
          disabled={isPending}
          id="agent-run-on"
          onChange={(event) => {
            discardModelProbe();
            onDraftChange({
              ...emptyWhereToRunDraft,
              runOn: event.target.value,
            });
          }}
          value={draft.runOn}
        >
          <option value="local">This computer</option>
          {backendProviders.map((provider) => (
            <option key={provider.id} value={provider.id}>
              {provider.id}
            </option>
          ))}
        </select>
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
                discardModelProbe();
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
        <select
          className="flex h-9 w-full rounded-md border border-input bg-background px-3 py-2 text-sm shadow-xs"
          disabled={isPending}
          id="agent-remote-harness"
          onChange={(event) => onSelect(event.target.value)}
          value={draft.remoteHarnessId ?? ""}
        >
          {available.map((harness) => (
            <option key={harness.id} value={harness.id}>
              {harness.label}
              {harness.version ? ` (${harness.version})` : ""}
            </option>
          ))}
        </select>
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
