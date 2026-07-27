import { providerRecordHarness } from "@/features/agents/lib/pinnedHarness";
import type { ManagedAgent } from "@/shared/api/types";

/**
 * How the profile surfaces name the harness behind an agent.
 *
 * One owner for a rule that was typed twice — the panel's "Runtime" field and
 * the popover's badge carried byte-identical copies of the table below, so a
 * name learned in one place was still wrong in the other.
 */

/**
 * Friendly names for the command strings a NON-record surface carries.
 *
 * The values here are a relay agent's self-declared `agentType` and a
 * definition's `runtime` preference: free-form strings from elsewhere, not a
 * pin this app can resolve. Unmatched input falls through to itself, which is
 * the honest answer for a name only its author knows.
 */
const RUNTIME_LABELS: Record<string, string> = {
  goose: "Goose",
  "claude-code": "Claude Code",
  "codex-acp": "Codex",
  aider: "Aider",
};

export function runtimeCommandLabel(command: string): string {
  return RUNTIME_LABELS[command] ?? command;
}

/**
 * The harness label for a managed record.
 *
 * A provider-backed record answers from its own pin: the table above cannot
 * name a binary on the HOST, and its misses rendered a raw `hermes` where a
 * name belonged — with no way to tell two profiles of one harness apart, since
 * the profile lives in the args. A LOCAL record keeps resolving exactly as it
 * did; the catalog genuinely describes this computer.
 */
export function managedAgentRuntimeLabel(
  agent: Pick<ManagedAgent, "backend" | "agentCommand" | "agentArgs">,
): string {
  return (
    providerRecordHarness(agent)?.label ??
    runtimeCommandLabel(agent.agentCommand)
  );
}

/**
 * What copying the harness field yields: the pin as it runs on the host,
 * args included, so a pasted command is one a human can actually run there.
 */
export function managedAgentRuntimeCopyValue(
  agent: Pick<ManagedAgent, "backend" | "agentCommand" | "agentArgs">,
): string {
  return providerRecordHarness(agent)?.command ?? agent.agentCommand;
}
