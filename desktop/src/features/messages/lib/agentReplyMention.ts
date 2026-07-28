import { getThreadReference } from "@/features/messages/lib/threading";
import type { RelayEvent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * The single agent pubkey a *typed* thread reply must `p`-tag to actually reach
 * the agent it is answering — or nothing, when the reply is not aimed at an
 * agent conversation at all.
 *
 * `messageMentionPubkeys` derives a stream message's recipients from the
 * composer's `@mentions` alone, so a reply typed into an agent's thread — the
 * numbered-list contract every ask publishes ("Reply with the number, your own
 * answer, or `!skip`", `crates/buzz-acp/src/acp.rs::render_elicitation_field`)
 * — carries no `p` tag at all. The harness subscribes with
 * `#p = [agent_pubkey]`, so the relay accepts and stores that reply, the owner
 * sees it land, and the agent never receives it: the question hangs until it
 * is cancelled. Card clicks were unaffected because `askReplyMentions` p-tags
 * the asking agent explicitly; only the typed path was silent. DMs were
 * unaffected too — that branch folds in every participant, which is why this
 * survived testing.
 *
 * Scope is deliberately narrow, so this never turns an ordinary channel
 * message into an agent wake-up:
 *
 *   - Replies only. A root-level message with no `@mention` still notifies
 *     nobody.
 *   - The parent event's *signer* (`event.pubkey`, never a relay-delegated
 *     display author) must be a known agent, resolved through the same
 *     `useKnownAgentPubkeys` baseline the ask card's trust gate uses.
 *   - At most one agent, and the parent always wins. The thread root is
 *     consulted *only* when the parent did not qualify, so a reply aimed at a
 *     human sibling inside an agent-rooted thread still reaches the agent that
 *     owns the conversation. Tagging both would mean that replying to agent B
 *     inside a thread agent A rooted wakes A too — an unsolicited full turn
 *     (pool slot, tokens, a reply nobody asked for). The harness deliberately
 *     narrowed that class on its side (`buzz-acp` matches an elicitation answer
 *     by `parent_event_id == question id` alone, so a root tag contributes
 *     nothing to the answer path); this must not re-widen it upstream of that
 *     gate.
 *
 * `findEvent` must be a synchronous cache read: this runs on the send path and
 * must never wait on the network. A parent that is not cached yields no
 * mention rather than a fetch — the reply still sends, and the owner keeps the
 * `@mention` fallback the composer has always offered.
 */
export function agentReplyMentionPubkeys(
  findEvent: (eventId: string) => RelayEvent | undefined,
  parentEventId: string | null | undefined,
  isKnownAgentPubkey: (pubkey: string) => boolean,
): string[] {
  if (!parentEventId) {
    return [];
  }

  const parent = findEvent(parentEventId);
  if (!parent) {
    return [];
  }

  const parentAgent = agentPubkeyOf(parent, isKnownAgentPubkey);
  if (parentAgent) {
    return [parentAgent];
  }

  const rootId = getThreadReference(parent.tags).rootId;
  if (!rootId || rootId === parent.id) {
    return [];
  }
  const root = findEvent(rootId);
  if (!root) {
    return [];
  }
  const rootAgent = agentPubkeyOf(root, isKnownAgentPubkey);
  return rootAgent ? [rootAgent] : [];
}

function agentPubkeyOf(
  event: RelayEvent,
  isKnownAgentPubkey: (pubkey: string) => boolean,
): string | null {
  const pubkey = normalizePubkey(event.pubkey);
  if (pubkey.length === 0 || !isKnownAgentPubkey(pubkey)) {
    return null;
  }
  return pubkey;
}
