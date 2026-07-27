import * as React from "react";
import { Check } from "lucide-react";
import { toast } from "sonner";

import { useAskAnswer } from "@/features/messages/AskAnswersContext";
import {
  type AskQuestion,
  askAcceleratorIndex,
  askAnswerLabels,
  askReplyContent,
  askRovingIndex,
  canAnswerAsk,
} from "@/features/messages/lib/askCard";
import { refreshChannelWindowMessages } from "@/features/messages/lib/projectChannelWindow";
import type { TimelineMessage } from "@/features/messages/types";
import { useIdentityQuery } from "@/shared/api/hooks";
import { sendChannelMessage } from "@/shared/api/tauri";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";
import { Input } from "@/shared/ui/input";
import { useQueryClient } from "@tanstack/react-query";

const SKIP_REPLY = "!skip";

/**
 * How stale a question may be and still take the keyboard on mount.
 *
 * A card claims focus when the question *arrives*, not when its row happens to
 * mount — scrolling back to an old question (or the virtualizer recycling its
 * row) must never yank the caret out of the composer.
 */
const ASK_FOCUS_WINDOW_SECONDS = 60;

/** Questions that have already taken the keyboard once, so a remount can't. */
const focusClaimedQuestionIds = new Set<string>();

/**
 * An agent's question, rendered as the clickable/keyboard-navigable card the
 * `["ask", …]` tag describes (`crates/buzz-acp/src/acp.rs`).
 *
 * Answering publishes an ordinary threaded reply whose content is exactly what
 * a typed answer would be — an option label, the option numbers for a
 * multi-select, or `!skip` — so the harness's interception (thread-parent
 * match on the question's event id) resolves it with no special case. The
 * reply is a NIP-CW broadcast so it lands on the channel window too: the card
 * derives "answered" from the timeline, which a thread-only reply never
 * reaches, and an un-collapsed card invites a second click the harness can
 * only read as a fresh prompt.
 *
 * Composed from `Button` and `Input`; the frame mirrors
 * `WorkflowApprovalCard` (bordered panel, disabled-while-pending actions) and
 * the send is the fire-and-forget `sendChannelMessage` call
 * `managedAgentControlActions.ts` uses for `!shutdown`.
 */
export function AskMessageCard({
  ask,
  channelId,
  message,
}: {
  ask: AskQuestion;
  channelId: string | null;
  message: TimelineMessage;
}) {
  const queryClient = useQueryClient();
  const questionId = message.id;
  const answer = useAskAnswer(questionId);
  const viewerPubkey = useIdentityQuery().data?.pubkey;
  // Read-only for everyone but the channel owner — including the asking
  // agent's own client. See `canAnswerAsk`.
  const interactive = canAnswerAsk(message, viewerPubkey);
  const [focusedIndex, setFocusedIndex] = React.useState(0);
  const [selected, setSelected] = React.useState<ReadonlySet<number>>(
    new Set(),
  );
  const [freeText, setFreeText] = React.useState("");
  // Latched on the first click. The harness's answer channel has capacity 1:
  // a second reply while the first is unconsumed falls through as an ordinary
  // prompt, so the controls must stay dead from the click, not from the ack.
  const [sending, setSending] = React.useState(false);
  const optionRefs = React.useRef<Array<HTMLButtonElement | null>>([]);
  const freeTextRef = React.useRef<HTMLInputElement | null>(null);

  const answered = answer !== null;
  const disabled = !interactive || answered || sending;
  const optionCount = ask.options.length;
  // `ownerPubkey` is null until the profile batch resolves, so at first paint
  // "not the owner" is not yet known — stay silent rather than tell the owner
  // their own card is read-only for a frame.
  let hint = "";
  if (interactive) {
    hint =
      optionCount > 0
        ? "↑↓ to move, 1–9 to pick, or reply in thread"
        : "Enter to answer, or reply in thread";
  } else if (message.ownerPubkey) {
    hint = "Waiting on the channel owner";
  }

  const send = React.useCallback(
    async (content: string) => {
      if (!channelId || !content.trim()) return;
      setSending(true);
      try {
        await sendChannelMessage(
          channelId,
          content,
          questionId,
          undefined,
          undefined,
          undefined,
          undefined,
          undefined,
          // Broadcast: the answered state is read off the channel window, and
          // a plain depth-1 reply is thread-only there (`NIP-CW`).
          true,
        );
        await refreshChannelWindowMessages(queryClient, channelId);
      } catch (error) {
        // Re-arm: nothing reached the relay, so the question is still open.
        setSending(false);
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to send your answer.",
        );
      }
    },
    [channelId, questionId, queryClient],
  );

  const commit = React.useCallback(
    (index: number) => {
      if (disabled) return;
      const label = ask.options[index]?.label;
      if (!label) return;
      if (!ask.multiSelect) {
        void send(label);
        return;
      }
      setSelected((current) => {
        const next = new Set(current);
        if (!next.delete(index)) next.add(index);
        return next;
      });
    },
    [ask.multiSelect, ask.options, disabled, send],
  );

  const focusOption = React.useCallback((index: number) => {
    setFocusedIndex(index);
    optionRefs.current[index]?.focus();
  }, []);

  // Like Claude Code's picker, a question takes the keyboard the moment it
  // arrives — otherwise the composer keeps focus and the accelerators below
  // type digits into the draft instead. Once per question and only while it is
  // fresh: scrolling back to an old card, or the virtualizer remounting this
  // row, must never yank the caret out of the composer.
  React.useEffect(() => {
    if (disabled || focusClaimedQuestionIds.has(questionId)) return;
    if (message.createdAt < Date.now() / 1000 - ASK_FOCUS_WINDOW_SECONDS) {
      return;
    }
    focusClaimedQuestionIds.add(questionId);
    // A question with nothing to click is answered in its own box.
    if (optionCount > 0) focusOption(0);
    else freeTextRef.current?.focus();
  }, [disabled, focusOption, message.createdAt, optionCount, questionId]);

  // Arrow keys wrap and digits jump, matching `handleMentionKeyDown`
  // (useMentions.ts) — the app's one established list-navigation contract.
  const handleKeyDown = React.useCallback(
    (event: React.KeyboardEvent<HTMLDivElement>) => {
      if (disabled) return;
      const count = optionCount;
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        focusOption(
          askRovingIndex(
            focusedIndex,
            event.key === "ArrowDown" ? 1 : -1,
            count,
          ),
        );
        return;
      }
      const accelerated = askAcceleratorIndex(event.key, count);
      if (accelerated !== null) {
        event.preventDefault();
        focusOption(accelerated);
        commit(accelerated);
      }
    },
    [commit, disabled, focusedIndex, focusOption, optionCount],
  );

  if (answered) {
    return (
      <div
        className="mt-1 flex max-w-md items-center gap-2 rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm text-muted-foreground"
        data-testid="ask-card-answered"
      >
        <Check aria-hidden="true" className="h-4 w-4 shrink-0" />
        <span className="min-w-0 truncate">
          {answer.content === SKIP_REPLY
            ? "Skipped"
            : `${interactive ? "You chose" : "Answered"}: ${askAnswerLabels(ask, answer.content)}`}
        </span>
      </div>
    );
  }

  return (
    // biome-ignore lint/a11y/useSemanticElements: a group of options, not a `<fieldset>` — there is no form to belong to.
    <div
      className="mt-1 max-w-md rounded-lg border border-border/70 bg-muted/30 p-3"
      data-testid="ask-card"
      onKeyDown={handleKeyDown}
      role="group"
    >
      {ask.total > 1 ? (
        <p className="mb-1 text-xs text-muted-foreground">
          Question {ask.index + 1} of {ask.total}
        </p>
      ) : null}
      <p className="mb-2 text-sm font-medium">{ask.question}</p>
      <div className="flex flex-col gap-1">
        {ask.options.map((option, index) => (
          <Button
            aria-pressed={ask.multiSelect ? selected.has(index) : undefined}
            className={cn(
              "h-auto justify-start gap-2 px-2 py-1.5 text-left",
              selected.has(index) && "bg-accent text-accent-foreground",
            )}
            disabled={disabled}
            key={option.label}
            onClick={() => commit(index)}
            ref={(element) => {
              optionRefs.current[index] = element;
            }}
            size="sm"
            tabIndex={index === focusedIndex ? 0 : -1}
            type="button"
            variant="ghost"
          >
            <span className="text-xs text-muted-foreground tabular-nums">
              {index + 1}
            </span>
            <span className="min-w-0 flex-1">
              <span className="block truncate font-medium">{option.label}</span>
              {option.description ? (
                <span className="block truncate text-xs text-muted-foreground">
                  {option.description}
                </span>
              ) : null}
            </span>
          </Button>
        ))}
      </div>
      {ask.allowFreeText ? (
        <Input
          aria-label="Your own answer"
          className="mt-2 h-8 text-sm"
          disabled={disabled}
          onChange={(event) => setFreeText(event.target.value)}
          onKeyDown={(event) => {
            // Always swallow: otherwise typing "1" here bubbles to the card's
            // accelerator handler and answers with option 1 instead.
            event.stopPropagation();
            if (event.key !== "Enter") return;
            event.preventDefault();
            void send(freeText);
          }}
          placeholder="Something else…"
          ref={freeTextRef}
          value={freeText}
        />
      ) : null}
      <div className="mt-2 flex items-center gap-2">
        {ask.multiSelect ? (
          <Button
            disabled={disabled || selected.size === 0}
            onClick={() =>
              void send(
                askReplyContent(
                  [...selected].sort((left, right) => left - right),
                ),
              )
            }
            size="sm"
            type="button"
          >
            Confirm
          </Button>
        ) : null}
        <Button
          disabled={disabled}
          onClick={() => void send(SKIP_REPLY)}
          size="sm"
          type="button"
          variant="ghost"
        >
          Skip
        </Button>
        <span className="ml-auto text-xs text-muted-foreground">{hint}</span>
      </div>
    </div>
  );
}
