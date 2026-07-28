import { getThreadReference } from "@/features/messages/lib/threading";
import type { TimelineMessage } from "@/features/messages/types";
import { KIND_STREAM_MESSAGE } from "@/shared/constants/kinds";
import type { RelayEvent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * Projectors for the agent-question card.
 *
 * `buzz-acp` publishes each question as an ordinary kind:9 whose body is a
 * numbered list (the answer contract every client understands) plus an
 * `["ask", <json>]` tag carrying the same question as structure. Clients that
 * ignore the tag show the list; we render a card. See
 * `crates/buzz-acp/src/acp.rs::elicitation_ask_tag`.
 *
 * Everything here is pure so the wire contract can be tested without a render
 * harness — the same split `configNudge.ts` uses for its fenced payload.
 */

export const ASK_TAG_NAME = "ask";

/**
 * Upper bound on the options one card will render.
 *
 * The harness caps the serialized tag at 4 KiB, but the producer is not the
 * trust boundary: any agent key can sign an `ask` tag, and the relay caps
 * kind:9 *content* without capping tags. A buggy or hostile agent must not be
 * able to turn one timeline row into thousands of buttons — over the bound the
 * card is refused and the numbered body renders instead.
 */
export const ASK_MAX_OPTIONS = 20;

export type AskOption = {
  label: string;
  description?: string;
};

export type AskQuestion = {
  question: string;
  options: AskOption[];
  /** `true` when the form field is an array — several labels, comma-joined. */
  multiSelect: boolean;
  /** `true` when an answer naming no option is still accepted. */
  allowFreeText: boolean;
  /** 0-based position within a multi-question form. */
  index: number;
  total: number;
};

function parseOption(value: unknown): AskOption | null {
  if (typeof value !== "object" || value === null) return null;
  const { label, description } = value as Record<string, unknown>;
  if (typeof label !== "string" || label.length === 0) return null;
  return typeof description === "string" && description.length > 0
    ? { label, description }
    : { label };
}

/**
 * Read the question structure off an event's tags, or `null` when the message
 * is not an agent question (every historical question, published before the
 * tag existed, lands here and renders as its markdown body — no migration).
 *
 * Rejects anything malformed rather than rendering a half-built card: the body
 * below is always a complete, answerable fallback.
 */
export function parseAskTag(
  tags: readonly string[][] | undefined,
): AskQuestion | null {
  const raw = tags?.find((tag) => tag[0] === ASK_TAG_NAME)?.[1];
  if (!raw) return null;

  let payload: unknown;
  try {
    payload = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof payload !== "object" || payload === null) return null;

  const { v, question, options, multiSelect, allowFreeText, index, total } =
    payload as Record<string, unknown>;
  if (v !== 1 || typeof question !== "string" || question.length === 0) {
    return null;
  }
  if (!Array.isArray(options) || options.length > ASK_MAX_OPTIONS) return null;

  const parsedOptions: AskOption[] = [];
  for (const option of options) {
    const parsed = parseOption(option);
    if (!parsed) return null;
    parsedOptions.push(parsed);
  }

  return {
    question,
    options: parsedOptions,
    multiSelect: multiSelect === true,
    // A question with no options is free-text whatever the payload claims —
    // a card offering neither buttons nor a text box is unanswerable.
    allowFreeText: allowFreeText === true || parsedOptions.length === 0,
    index: typeof index === "number" ? index : 0,
    total: typeof total === "number" ? total : 1,
  };
}

/**
 * The question a message carries, or `null` when it is not an authentic agent
 * question. Mirrors `getConfigNudgeAuthorPubkey`: any channel member could post
 * an `ask` tag with attacker-chosen labels, and a click on the resulting card
 * would publish attacker-chosen text as the owner — so the card renders only
 * for a kind:9 whose *signer* (not a relay-delegated display author) is a known
 * agent.
 */
export function resolveAskQuestion(
  message: Pick<TimelineMessage, "kind" | "signerPubkey" | "tags">,
  isKnownAgentPubkey: (pubkey: string) => boolean,
): AskQuestion | null {
  if (
    message.kind !== KIND_STREAM_MESSAGE ||
    !message.signerPubkey ||
    !isKnownAgentPubkey(message.signerPubkey)
  ) {
    return null;
  }
  return parseAskTag(message.tags);
}

/**
 * Whether this viewer's clicks would actually answer the question.
 *
 * The harness intercepts only the channel owner's reply; anyone else's click
 * publishes a bare label that reaches the agent as a fresh prompt. The asking
 * agent's own client is never interactive either — it has nobody to answer.
 */
export function canAnswerAsk(
  message: Pick<TimelineMessage, "ownerPubkey" | "signerPubkey">,
  viewerPubkey: string | undefined,
): boolean {
  if (!viewerPubkey || !message.ownerPubkey) return false;
  const viewer = normalizePubkey(viewerPubkey);
  return (
    viewer === normalizePubkey(message.ownerPubkey) &&
    viewer !== normalizePubkey(message.signerPubkey ?? "")
  );
}

export type AskAnswer = {
  content: string;
  pubkey: string;
};

/**
 * Index the answer to each question, keyed by the question's event id.
 *
 * The harness disarms a question internally and publishes nothing on
 * resolution, so the reply itself is the only "answered" signal on the wire.
 * The earliest one wins: the answer channel has capacity 1, so anything after
 * it reached the agent as an ordinary prompt, not as this question's answer.
 *
 * Only the reply the harness would accept counts — it is owner-gated, and the
 * question p-tags exactly that owner, so a bystander replying in the thread
 * must not collapse a card that is still waiting.
 */
export function buildAskAnswerIndex(
  events: readonly RelayEvent[],
): ReadonlyMap<string, AskAnswer> {
  const answererByQuestionId = new Map<string, string>();
  for (const event of events) {
    if (!event.tags.some((tag) => tag[0] === ASK_TAG_NAME)) continue;
    const owner = event.tags.find((tag) => tag[0] === "p")?.[1];
    if (owner) answererByQuestionId.set(event.id, normalizePubkey(owner));
  }
  if (answererByQuestionId.size === 0) return new Map();

  const answers = new Map<string, AskAnswer & { createdAt: number }>();
  for (const event of events) {
    const parentId = getThreadReference(event.tags).parentId;
    if (!parentId) continue;
    if (answererByQuestionId.get(parentId) !== normalizePubkey(event.pubkey)) {
      continue;
    }
    const existing = answers.get(parentId);
    if (existing && existing.createdAt <= event.created_at) continue;
    answers.set(parentId, {
      content: event.content,
      pubkey: event.pubkey,
      createdAt: event.created_at,
    });
  }
  return answers;
}

/** Options are numbered from 1 in the body; `1`–`9` jump straight to one. */
export function askAcceleratorIndex(
  key: string,
  optionCount: number,
): number | null {
  if (key.length !== 1 || key < "1" || key > "9") return null;
  const index = Number(key) - 1;
  return index < optionCount ? index : null;
}

/** Wrapping roving-focus step, matching `handleMentionKeyDown`'s semantics. */
export function askRovingIndex(
  current: number,
  delta: number,
  count: number,
): number {
  if (count === 0) return 0;
  return (current + delta + count) % count;
}

/**
 * The message body a multi-select confirmation sends: 1-based option numbers,
 * comma-separated — exactly what the body's "Reply with the numbers
 * (comma-separated)" instruction asks a typing owner for.
 *
 * Numbers, not labels, because `answer_elicitation_field` splits an array reply
 * on `,` before resolving each token: a label containing a comma
 * ("Yes, immediately") would split into tokens that match no option, and the
 * agent would silently receive free strings instead of the option's wire value.
 * `ElicitationField::select` resolves a 1-based index first, so numbers are
 * unambiguous whatever the labels contain.
 */
export function askReplyContent(indices: readonly number[]): string {
  return indices.map((index) => index + 1).join(", ");
}

/**
 * The pubkeys an answer must p-tag: the agent that asked, and only it.
 *
 * The harness subscribes with `#p = [agent_pubkey]` (`BUZZ_ACP_SUBSCRIBE`
 * defaults to mentions), so an answer carrying no `p` tag is accepted and
 * stored by the relay — the card collapses, the owner sees success — but is
 * never delivered over the agent's REQ, and the question hangs until it is
 * cancelled. `signerPubkey` is the key in that filter, and it is already the
 * trust anchor `resolveAskQuestion` gates the card on.
 */
export function askReplyMentions(
  message: Pick<TimelineMessage, "signerPubkey">,
): string[] | undefined {
  return message.signerPubkey ? [message.signerPubkey] : undefined;
}

/**
 * What the answered row shows for a reply — the option labels behind it, so a
 * numbered multi-select answer reads as "Postgres, SQLite" rather than "1, 2".
 *
 * Resolves each token the way the harness does (`ElicitationField::select`):
 * 1-based index first, then a case-insensitive label match. A reply naming no
 * option is free text and is shown verbatim, which is also what the agent
 * received.
 */
export function askAnswerLabels(ask: AskQuestion, content: string): string {
  const tokens = (ask.multiSelect ? content.split(",") : [content]).map(
    (token) => token.trim(),
  );
  const labels = tokens.map((token) => {
    const index = Number(token);
    if (Number.isInteger(index) && index >= 1 && index <= ask.options.length) {
      return ask.options[index - 1].label;
    }
    return (
      ask.options.find(
        (option) => option.label.toLowerCase() === token.toLowerCase(),
      )?.label ?? token
    );
  });
  return labels.join(", ");
}
