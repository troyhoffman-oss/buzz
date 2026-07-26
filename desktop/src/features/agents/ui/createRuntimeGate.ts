import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";
import {
  formatRuntimeOptionLabel,
  NO_RUNTIME_DROPDOWN_VALUE,
  type PersonaDropdownOption,
  sortPersonaRuntimes,
} from "./agentConfigOptions";

/**
 * How much the LOCAL runtime catalog is allowed to gate a definition create.
 *
 * A local create must name a runtime installed on this computer. A remote
 * create must not: its harness comes from the host's catalog via the
 * "Where to run" section, and the local catalog describes a different machine
 * entirely — requiring a local install would make every remote-only harness
 * unsubmittable (no Goose agent on a server without Goose on the laptop).
 *
 * `createSubmitBlocked` still gates the remote case; it is false only once a
 * remote harness has actually been picked.
 */
export type CreateRuntimeGateInput = {
  isCreateMode: boolean;
  /** "Where to run" targets a backend provider. */
  runsRemotely: boolean;
  /** The definition's runtime id, as typed/selected. */
  runtime: string;
  selectedRuntime: AcpRuntimeCatalogEntry | null | undefined;
  /** True when the app could resolve any default runtime locally. */
  hasLocalDefaultRuntime: boolean;
};

/** Whether the picked runtime clears the local-availability requirement. */
export function createRuntimeIsAvailable({
  runsRemotely,
  runtime,
  selectedRuntime,
}: Pick<
  CreateRuntimeGateInput,
  "runsRemotely" | "runtime" | "selectedRuntime"
>): boolean {
  if (runsRemotely) return true;
  if (runtime.trim().length === 0) return true;
  return selectedRuntime?.availability === "available";
}

/** Whether the runtime field satisfies the create-mode requirements. */
export function createRuntimeSelectionSatisfied(
  input: CreateRuntimeGateInput,
): boolean {
  if (!input.isCreateMode) return true;
  if (input.runsRemotely) return true;
  return input.runtime.trim().length > 0 && createRuntimeIsAvailable(input);
}

/**
 * Whether an unavailable runtime option should be unselectable. Remote creates
 * never disable an option: availability here describes the wrong machine.
 */
export function createRuntimeOptionDisabled(
  candidate: AcpRuntimeCatalogEntry,
  input: Pick<
    CreateRuntimeGateInput,
    "isCreateMode" | "runsRemotely" | "hasLocalDefaultRuntime"
  >,
): boolean {
  return (
    input.isCreateMode &&
    !input.runsRemotely &&
    input.hasLocalDefaultRuntime &&
    candidate.availability !== "available"
  );
}

/**
 * Label for the "no explicit runtime" state: the placeholder in create mode,
 * and an actual selectable option when editing (where blank is legitimate).
 */
export function runtimeDropdownPlaceholder({
  isCreateMode,
  runtimesLoading,
}: {
  isCreateMode: boolean;
  runtimesLoading: boolean;
}): string {
  if (runtimesLoading) return "Loading harnesses...";
  return isCreateMode ? "Choose a harness" : "No preference (use app default)";
}

/**
 * The harness dropdown for the definition dialog: catalog order, the gate's
 * disabled flags, and a trailing entry for a runtime the catalog no longer
 * knows so an existing definition never silently loses its own value.
 */
export function runtimeDropdownOptions({
  gate,
  defaultRuntimeId,
  runtimes,
  runtimesLoading,
}: {
  gate: CreateRuntimeGateInput;
  defaultRuntimeId: string | null;
  runtimes: readonly AcpRuntimeCatalogEntry[];
  runtimesLoading: boolean;
}): PersonaDropdownOption[] {
  const options: PersonaDropdownOption[] = [
    ...(gate.isCreateMode
      ? []
      : [
          {
            label: runtimeDropdownPlaceholder({
              isCreateMode: gate.isCreateMode,
              runtimesLoading,
            }),
            value: NO_RUNTIME_DROPDOWN_VALUE,
          },
        ]),
    ...sortPersonaRuntimes(runtimes).map((candidate) => ({
      disabled: createRuntimeOptionDisabled(candidate, gate),
      label: `${formatRuntimeOptionLabel(candidate)}${
        gate.isCreateMode && candidate.id === defaultRuntimeId
          ? " (default)"
          : ""
      }`,
      value: candidate.id,
    })),
  ];
  const current = gate.runtime.trim();
  if (
    current.length > 0 &&
    !options.some((option) => option.value === gate.runtime)
  ) {
    options.push({ label: `${current} (current)`, value: current });
  }
  return options;
}
