/**
 * Wire-contract tests for the agent-question card.
 *
 * The harness (crates/buzz-acp/src/acp.rs) publishes each question as a kind:9
 * carrying an `["ask", <json>]` tag; every projector here reads that tag or the
 * replies threaded under it. Keeping them pure means the contract is locked
 * without a render harness — the same split configNudge.test.mjs uses.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  ASK_MAX_OPTIONS,
  askAcceleratorIndex,
  askAnswerLabels,
  askReplyContent,
  askReplyMentions,
  askRovingIndex,
  buildAskAnswerIndex,
  canAnswerAsk,
  parseAskTag,
  resolveAskQuestion,
} from "./askCard.ts";

const AGENT = `${"a".repeat(64)}`;
const OWNER = `${"b".repeat(64)}`;
const STRANGER = `${"c".repeat(64)}`;
const QUESTION_ID = `${"d".repeat(64)}`;

function askPayload(overrides = {}) {
  return JSON.stringify({
    v: 1,
    question: "Which database?",
    options: [
      { label: "Postgres", description: "mature" },
      { label: "SQLite" },
    ],
    multiSelect: false,
    allowFreeText: true,
    index: 0,
    total: 1,
    ...overrides,
  });
}

function askTags(overrides = {}) {
  return [
    ["h", "channel"],
    // The harness p-tags the owner it expects the answer from.
    ["p", OWNER],
    ["ask", askPayload(overrides)],
  ];
}

function replyEvent({ id, parentId, content, pubkey, createdAt }) {
  return {
    id,
    pubkey,
    kind: 9,
    created_at: createdAt,
    content,
    tags: [
      ["h", "channel"],
      ["e", parentId, "", "reply"],
    ],
    sig: "sig",
  };
}

test("parseAskTag reads the harness payload", () => {
  const ask = parseAskTag(askTags());
  assert.equal(ask.question, "Which database?");
  assert.deepEqual(ask.options, [
    { label: "Postgres", description: "mature" },
    { label: "SQLite" },
  ]);
  assert.equal(ask.multiSelect, false);
  assert.equal(ask.allowFreeText, true);
  assert.equal(ask.total, 1);
});

test("parseAskTag ignores untagged and malformed messages", () => {
  // Historical questions predate the tag: they must fall through to markdown.
  assert.equal(parseAskTag([["h", "channel"]]), null);
  assert.equal(parseAskTag(undefined), null);
  assert.equal(parseAskTag([["ask", "not json"]]), null);
  assert.equal(parseAskTag([["ask", askPayload({ v: 2 })]]), null);
  assert.equal(parseAskTag([["ask", askPayload({ question: "" })]]), null);
  assert.equal(parseAskTag([["ask", askPayload({ options: "nope" })]]), null);
  assert.equal(
    parseAskTag([["ask", askPayload({ options: [{ title: "no label" }] })]]),
    null,
    "a half-built card is worse than the answerable body below it",
  );
});

test("parseAskTag forces free text when there is nothing to click", () => {
  const ask = parseAskTag([
    ["ask", askPayload({ options: [], allowFreeText: false })],
  ]);
  assert.equal(ask.allowFreeText, true);
});

test("resolveAskQuestion authenticates the signer, not the display author", () => {
  const isAgent = (pubkey) => pubkey === AGENT;
  const message = { kind: 9, signerPubkey: AGENT, tags: askTags() };
  assert.ok(resolveAskQuestion(message, isAgent));
  assert.equal(
    resolveAskQuestion({ ...message, signerPubkey: STRANGER }, isAgent),
    null,
    "a member forging an ask tag must not get a clickable card",
  );
  assert.equal(resolveAskQuestion({ ...message, kind: 40008 }, isAgent), null);
});

test("canAnswerAsk allows only the owner, never the asking agent", () => {
  const message = { ownerPubkey: OWNER, signerPubkey: AGENT };
  assert.equal(canAnswerAsk(message, OWNER), true);
  assert.equal(canAnswerAsk(message, OWNER.toUpperCase()), true);
  assert.equal(canAnswerAsk(message, STRANGER), false);
  assert.equal(canAnswerAsk(message, undefined), false);
  assert.equal(
    canAnswerAsk({ ownerPubkey: AGENT, signerPubkey: AGENT }, AGENT),
    false,
    "the agent's own client renders its question read-only",
  );
});

test("an answer p-tags the asking agent so the harness receives it", () => {
  // The harness subscribes with `#p = [agent_pubkey]`. An answer with no `p`
  // tag is stored by the relay and collapses the card, but never reaches the
  // agent's REQ — the question then hangs until it is cancelled.
  assert.deepEqual(askReplyMentions({ signerPubkey: AGENT }), [AGENT]);
  assert.equal(askReplyMentions({ signerPubkey: undefined }), undefined);
  assert.equal(askReplyMentions({ signerPubkey: "" }), undefined);
  assert.equal(askReplyMentions({}), undefined);
});

test("buildAskAnswerIndex takes the owner's earliest reply", () => {
  const events = [
    {
      id: QUESTION_ID,
      pubkey: AGENT,
      kind: 9,
      created_at: 10,
      content: "**Which database?**",
      tags: askTags(),
      sig: "sig",
    },
    replyEvent({
      id: "r2",
      parentId: QUESTION_ID,
      content: "SQLite",
      pubkey: OWNER,
      createdAt: 30,
    }),
    replyEvent({
      id: "r1",
      parentId: QUESTION_ID,
      content: "Postgres",
      pubkey: OWNER,
      createdAt: 20,
    }),
  ];

  const answers = buildAskAnswerIndex(events);
  assert.equal(answers.get(QUESTION_ID).content, "Postgres");
  assert.equal(answers.get(QUESTION_ID).pubkey, OWNER);
});

test("buildAskAnswerIndex ignores replies the harness would not accept", () => {
  const question = {
    id: QUESTION_ID,
    pubkey: AGENT,
    kind: 9,
    created_at: 10,
    content: "**Which database?**",
    tags: askTags(),
    sig: "sig",
  };
  const events = [
    question,
    replyEvent({
      id: "r1",
      parentId: "some-other-message",
      content: "hi",
      pubkey: OWNER,
      createdAt: 20,
    }),
    // Interception is owner-gated: a bystander chiming in on the thread reaches
    // the agent as an ordinary prompt, so the question is still open.
    replyEvent({
      id: "r2",
      parentId: QUESTION_ID,
      content: "Postgres",
      pubkey: STRANGER,
      createdAt: 20,
    }),
  ];
  assert.equal(buildAskAnswerIndex(events).size, 0);
});

test("accelerators map 1-9 onto the body's numbering, and nothing else", () => {
  assert.equal(askAcceleratorIndex("1", 2), 0);
  assert.equal(askAcceleratorIndex("2", 2), 1);
  assert.equal(askAcceleratorIndex("3", 2), null, "past the last option");
  assert.equal(askAcceleratorIndex("0", 9), null);
  assert.equal(askAcceleratorIndex("a", 9), null);
  assert.equal(askAcceleratorIndex("Enter", 9), null);
});

test("roving focus wraps in both directions", () => {
  assert.equal(askRovingIndex(0, 1, 3), 1);
  assert.equal(askRovingIndex(2, 1, 3), 0);
  assert.equal(askRovingIndex(0, -1, 3), 2);
  assert.equal(askRovingIndex(0, 1, 0), 0);
});

test("multi-select replies send option numbers, not labels", () => {
  // `answer_elicitation_field` splits an array reply on ',' before resolving
  // each token, so a label containing a comma would resolve to nothing.
  // `ElicitationField::select` takes a 1-based index first, so numbers survive
  // any label.
  assert.equal(askReplyContent([0, 1]), "1, 2");
  assert.equal(askReplyContent([0]), "1");
});

test("the answered row shows labels behind a numbered reply", () => {
  const ask = parseAskTag(askTags({ multiSelect: true }));
  assert.equal(askAnswerLabels(ask, "1, 2"), "Postgres, SQLite");
  // Typed answers still resolve, and free text is shown as the agent got it.
  assert.equal(askAnswerLabels(ask, "postgres"), "Postgres");
  assert.equal(askAnswerLabels(ask, "9"), "9");
  assert.equal(askAnswerLabels(ask, "neither, actually"), "neither, actually");
});

test("a card refuses more options than it will render", () => {
  const options = Array.from({ length: ASK_MAX_OPTIONS }, (_, index) => ({
    label: `option ${index}`,
  }));
  assert.equal(
    parseAskTag(askTags({ options })).options.length,
    ASK_MAX_OPTIONS,
  );
  // One over the bound falls back to the numbered body rather than painting a
  // timeline row with unbounded buttons — the signing agent is not trusted.
  assert.equal(
    parseAskTag(askTags({ options: [...options, { label: "x" }] })),
    null,
  );
});
