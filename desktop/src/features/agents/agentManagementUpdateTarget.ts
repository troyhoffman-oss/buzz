import { providerRecordHarness } from "./lib/pinnedHarness";
import type { ManagedAgent } from "@/shared/api/types";

/**
 * The provider-backed record an owner-reviewed `update` draft should be edited
 * through, or `null` when the definition dialog is still the right surface.
 *
 * Rule 19 again, on the second door into the same dialog: a `!model` request
 * resolves a definition by name and opens `definition-edit`, but a
 * provider-backed record's harness, command and model live on the HOST and the
 * definition projection carries none of them. The instance dialog is the only
 * surface that reads the record itself, so a remote target edits there.
 *
 * Deliberately strict — an ambiguous name yields `null` rather than a guess,
 * matching the "more than one personal agent has that name" refusal the
 * definition path already surfaces.
 */
export function agentManagementUpdateTarget({
  agents,
  agentName,
  personaId,
}: {
  agents: readonly ManagedAgent[] | undefined;
  agentName: string;
  personaId: string | undefined;
}): ManagedAgent | null {
  const target = agentName.trim().toLocaleLowerCase();
  const matches = (agents ?? []).filter(
    (agent) =>
      (personaId !== undefined && agent.personaId === personaId) ||
      agent.name.trim().toLocaleLowerCase() === target,
  );
  if (matches.length !== 1) return null;
  const [only] = matches;
  return providerRecordHarness(only) ? only : null;
}
