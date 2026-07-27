import * as React from "react";

import { type AskAnswer, buildAskAnswerIndex } from "./lib/askCard";
import type { RelayEvent } from "@/shared/api/types";

/**
 * First reply to each agent question in the active channel, keyed by the
 * question's event id — the only "answered" signal on the wire, since the
 * harness resolves a question without publishing anything.
 *
 * Provided once per channel rather than derived per row: every ask card in the
 * timeline and the thread panel needs the same scan, and `MessageRow` has no
 * channel-wide event list of its own. Same shape as
 * `ChannelNavigationContext`, which exists for the same reason.
 */
const AskAnswersContext = React.createContext<ReadonlyMap<string, AskAnswer>>(
  new Map(),
);

export function AskAnswersProvider({
  channelEvents,
  threadReplyEvents,
  children,
}: {
  channelEvents: readonly RelayEvent[];
  threadReplyEvents: readonly RelayEvent[];
  children: React.ReactNode;
}) {
  const value = React.useMemo(
    // A question is threaded to its trigger, so its answer may arrive in the
    // channel window or only in an opened thread's page — scan both.
    () => buildAskAnswerIndex([...channelEvents, ...threadReplyEvents]),
    [channelEvents, threadReplyEvents],
  );

  return (
    <AskAnswersContext.Provider value={value}>
      {children}
    </AskAnswersContext.Provider>
  );
}

export function useAskAnswer(questionId: string): AskAnswer | null {
  return React.useContext(AskAnswersContext).get(questionId) ?? null;
}
