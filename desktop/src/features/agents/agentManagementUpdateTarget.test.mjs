import assert from "node:assert/strict";
import test from "node:test";

import { agentManagementUpdateTarget } from "./agentManagementUpdateTarget.ts";

function agent(overrides = {}) {
  return {
    pubkey: "aa",
    name: "Scout",
    personaId: "persona-1",
    backend: { type: "local" },
    agentCommand: "buzz-agent",
    agentArgs: [],
    ...overrides,
  };
}

const remote = agent({
  backend: { type: "provider", id: "ssh", config: {} },
  agentCommand: "/opt/homebrew/bin/goose",
  agentArgs: ["acp"],
});

test("a local record keeps the definition-edit path", () => {
  assert.equal(
    agentManagementUpdateTarget({
      agents: [agent()],
      agentName: "Scout",
      personaId: "persona-1",
    }),
    null,
  );
});

test("a provider-backed record selects the instance editor", () => {
  assert.equal(
    agentManagementUpdateTarget({
      agents: [remote],
      agentName: "Scout",
      personaId: "persona-1",
    }),
    remote,
  );
});

test("a name-only match still resolves when no definition was found", () => {
  assert.equal(
    agentManagementUpdateTarget({
      agents: [remote],
      agentName: "  scout  ",
      personaId: undefined,
    }),
    remote,
    "the name is matched case- and whitespace-insensitively, as the persona path does",
  );
});

test("an ambiguous target is never guessed", () => {
  assert.equal(
    agentManagementUpdateTarget({
      agents: [remote, agent({ pubkey: "bb" })],
      agentName: "Scout",
      personaId: "persona-1",
    }),
    null,
    "two records for one name refuse, matching the definition path's refusal",
  );
});

test("an unloaded agent list resolves nothing", () => {
  assert.equal(
    agentManagementUpdateTarget({
      agents: undefined,
      agentName: "Scout",
      personaId: "persona-1",
    }),
    null,
  );
});
