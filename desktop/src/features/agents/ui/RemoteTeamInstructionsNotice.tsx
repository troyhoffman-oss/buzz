import {
  losesTeamInstructionsRemotely,
  REMOTE_TEAM_INSTRUCTIONS_ACTIVE_NOTICE,
  REMOTE_TEAM_INSTRUCTIONS_NOTICE,
} from "@/features/agents/lib/remoteTeamInstructions";
import type { ManagedAgentBackend } from "@/shared/api/types";

/**
 * The two renderings of one disclosure: a remote agent does not receive its
 * team's instructions.
 *
 * `remoteTeamInstructions` owns the fact and the copy; this file owns how each
 * surface shows it, so the create flow and the edit flow cannot drift into two
 * different presentations of the same limitation.
 */

/**
 * The create flow's rendering: stated the moment "elsewhere" is the answer.
 *
 * Unconditional, because the team is chosen *after* "Where to run" — waiting
 * for one to be picked would surface the limitation only where it is already
 * too late to weigh. Muted: at this point it is a property of the choice, not
 * a live condition.
 */
export function RemoteTeamInstructionsHint() {
  return (
    <p
      className="text-xs text-muted-foreground"
      data-testid="remote-team-instructions-notice"
    >
      {REMOTE_TEAM_INSTRUCTIONS_NOTICE}
    </p>
  );
}

/**
 * The edit flow's rendering: this record is running without its team's rules
 * right now.
 *
 * Rendered only for the records the limitation actually bites, and in the
 * warning tone, because here it describes present behaviour rather than a
 * consequence of a choice still being made.
 */
export function RemoteTeamInstructionsNotice({
  agent,
}: {
  agent: { backend?: ManagedAgentBackend | null; teamId?: string | null };
}) {
  if (!losesTeamInstructionsRemotely(agent)) {
    return null;
  }
  return (
    <p
      className="text-xs text-warning"
      data-testid="edit-agent-remote-team-instructions-notice"
    >
      {REMOTE_TEAM_INSTRUCTIONS_ACTIVE_NOTICE}
    </p>
  );
}
