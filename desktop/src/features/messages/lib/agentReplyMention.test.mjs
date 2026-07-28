import assert from "node:assert/strict";
import test from "node:test";

import { agentReplyMentionPubkeys } from "./agentReplyMention.ts";

const AGENT = "a".repeat(64);
const OWNER = "b".repeat(64);
const OTHER_HUMAN = "c".repeat(64);

const isAgent = (pubkey) => pubkey === AGENT;

function event(id, pubkey, tags = []) {
  return {
    id,
    pubkey,
    created_at: 0,
    kind: 9,
    tags,
    content: "",
    sig: "",
  };
}

/** Cache reader over a fixed event list, the way the send path reads caches. */
function lookup(events) {
  return (eventId) => events.find((candidate) => candidate.id === eventId);
}

test("a reply to an agent's question p-tags that agent", () => {
  const events = [event("ask-1", AGENT, [["h", "chan"]])];
  assert.deepEqual(agentReplyMentionPubkeys(lookup(events), "ask-1", isAgent), [
    AGENT,
  ]);
});

test("a root-level message p-tags nobody", () => {
  const events = [event("ask-1", AGENT)];
  assert.deepEqual(agentReplyMentionPubkeys(lookup(events), null, isAgent), []);
  assert.deepEqual(
    agentReplyMentionPubkeys(lookup(events), undefined, isAgent),
    [],
  );
});

test("a reply to a human's message p-tags nobody", () => {
  const events = [event("msg-1", OTHER_HUMAN)];
  assert.deepEqual(
    agentReplyMentionPubkeys(lookup(events), "msg-1", isAgent),
    [],
  );
});

test("a reply to a human sibling inside an agent-rooted thread reaches the agent", () => {
  const events = [
    event("root-1", AGENT),
    event("reply-1", OTHER_HUMAN, [
      ["e", "root-1", "", "root"],
      ["e", "root-1", "", "reply"],
    ]),
  ];
  assert.deepEqual(
    agentReplyMentionPubkeys(lookup(events), "reply-1", isAgent),
    [AGENT],
  );
});

test("a human-rooted thread with an agent parent p-tags only the agent", () => {
  const events = [
    event("root-1", OWNER),
    event("agent-reply", AGENT, [
      ["e", "root-1", "", "root"],
      ["e", "root-1", "", "reply"],
    ]),
  ];
  assert.deepEqual(
    agentReplyMentionPubkeys(lookup(events), "agent-reply", isAgent),
    [AGENT],
  );
});

test("an agent parent inside its own thread p-tags that agent once", () => {
  const events = [
    event("root-1", AGENT),
    event("agent-reply", AGENT, [
      ["e", "root-1", "", "root"],
      ["e", "root-1", "", "reply"],
    ]),
  ];
  assert.deepEqual(
    agentReplyMentionPubkeys(lookup(events), "agent-reply", isAgent),
    [AGENT],
  );
});

test("an agent parent under a different agent's root p-tags only the parent", () => {
  const secondAgent = "d".repeat(64);
  const events = [
    event("root-1", AGENT),
    event("agent-reply", secondAgent, [
      ["e", "root-1", "", "root"],
      ["e", "root-1", "", "reply"],
    ]),
  ];
  // Tagging the root agent too would wake an agent nobody addressed: a full
  // turn, a pool slot, and a spurious reply. The root is a fallback for a
  // non-agent parent, never an addition to one.
  assert.deepEqual(
    agentReplyMentionPubkeys(
      lookup(events),
      "agent-reply",
      (pubkey) => pubkey === AGENT || pubkey === secondAgent,
    ),
    [secondAgent],
  );
});

test("a qualifying agent parent short-circuits the thread-root lookup", () => {
  const reads = [];
  const events = [
    event("root-1", AGENT),
    event("agent-reply", AGENT, [
      ["e", "root-1", "", "root"],
      ["e", "root-1", "", "reply"],
    ]),
  ];
  const find = (eventId) => {
    reads.push(eventId);
    return lookup(events)(eventId);
  };
  assert.deepEqual(agentReplyMentionPubkeys(find, "agent-reply", isAgent), [
    AGENT,
  ]);
  assert.deepEqual(reads, ["agent-reply"], "the root is never read");
});

test("pubkeys are normalized before the agent check and the tag", () => {
  const events = [event("ask-1", `  ${AGENT.toUpperCase()} `)];
  assert.deepEqual(agentReplyMentionPubkeys(lookup(events), "ask-1", isAgent), [
    AGENT,
  ]);
});

test("an uncached parent yields no mention and never fetches", () => {
  let reads = 0;
  const find = (eventId) => {
    reads += 1;
    return eventId === "cached" ? event("cached", AGENT) : undefined;
  };
  assert.deepEqual(agentReplyMentionPubkeys(find, "missing", isAgent), []);
  assert.equal(reads, 1, "a miss stops at the parent lookup");
});

test("an uncached thread root under a human parent yields no mention", () => {
  const events = [
    event("human-reply", OTHER_HUMAN, [
      ["e", "root-1", "", "root"],
      ["e", "root-1", "", "reply"],
    ]),
  ];
  assert.deepEqual(
    agentReplyMentionPubkeys(lookup(events), "human-reply", isAgent),
    [],
  );
});
