import { providerRecordHarness } from "@/features/agents/lib/pinnedHarness";
import type { AgentPersona, ManagedAgent } from "@/shared/api/types";

/** Which editor the profile panel's "Edit agent" action opens. */
export type ProfileEditAgentTarget = "instance" | "definition";

/**
 * Route the profile panel's Edit action to the editor that can actually show
 * this agent's configuration.
 *
 * Rule 19 (`features/agents/AGENTS.md`): a provider record answers from itself.
 * Its harness, command and model describe the HOST, and the definition
 * projection drops all three by design (`ManagedAgentRecord::to_definition_view`
 * has no slot for `backend` or `agent_command`), so the definition dialog opens
 * on a blank runtime and re-seeds it from this computer's catalog — showing a
 * remote agent as running a local harness it has never run. `providerRecordHarness`
 * stays the single owner of that question, so a local persona-backed agent takes
 * the definition path exactly as before.
 */
export function profileEditAgentTarget({
  managedAgent,
  resolvedPersona,
}: {
  managedAgent: ManagedAgent | undefined;
  resolvedPersona: AgentPersona | undefined;
}): ProfileEditAgentTarget {
  if (managedAgent && providerRecordHarness(managedAgent)) return "instance";
  return resolvedPersona ? "definition" : "instance";
}
