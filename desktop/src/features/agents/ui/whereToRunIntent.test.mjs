import assert from "node:assert/strict";
import test from "node:test";

import {
  canSubmitWhereToRun,
  emptyWhereToRunDraft,
  providerConfigComplete,
  resolveBackendIntent,
  selectedRemoteHarness,
} from "./whereToRunIntent.ts";

const probed = {
  ok: true,
  config_schema: {
    properties: { region: { type: "string" }, size: { type: "integer" } },
    required: ["region"],
  },
};

const gooseHarness = {
  id: "goose",
  label: "Goose",
  command: "/opt/host/bin/goose",
  args: ["acp"],
  env: { GOOSE_MODE: "auto" },
  available: true,
  binaryPath: "/opt/host/bin/goose",
  version: "1.2.0",
};

function providerDraft(overrides = {}) {
  return {
    ...emptyWhereToRunDraft,
    runOn: "blox",
    probedProvider: probed,
    providerConfig: { region: "us", size: "3" },
    remoteHarnesses: [gooseHarness],
    remoteHarnessId: "goose",
    ...overrides,
  };
}

test("provider selection blocks submit until the probe completes", () => {
  assert.equal(
    canSubmitWhereToRun(providerDraft({ probedProvider: null })),
    false,
  );
});

test("provider selection blocks submit while required config is missing", () => {
  const missing = providerDraft({ providerConfig: { size: "3" } });
  assert.equal(canSubmitWhereToRun(missing), false);
  assert.equal(providerConfigComplete(missing), false);
});

test("complete provider config allows submit", () => {
  assert.equal(canSubmitWhereToRun(providerDraft()), true);
});

test("local never gates submit", () => {
  assert.equal(canSubmitWhereToRun(emptyWhereToRunDraft), true);
});

test("local draft resolves to null intent", () => {
  assert.equal(resolveBackendIntent(emptyWhereToRunDraft), null);
});

test("provider draft resolves with coerced config values and the remote harness", () => {
  const intent = resolveBackendIntent(providerDraft());
  assert.deepEqual(intent, {
    type: "provider",
    id: "blox",
    config: { region: "us", size: 3 },
    harness: {
      id: "goose",
      command: "/opt/host/bin/goose",
      args: ["acp"],
      env: { GOOSE_MODE: "auto" },
    },
  });
});

// Correction C1: the harness pin is the only channel by which the choice
// reaches the host, so submit must stay blocked until one is picked rather
// than letting the create fall back to the locally-resolved default.
test("provider selection blocks submit until a remote harness is picked", () => {
  assert.equal(
    canSubmitWhereToRun(
      providerDraft({ remoteHarnesses: null, remoteHarnessId: null }),
    ),
    false,
  );
  assert.equal(
    canSubmitWhereToRun(providerDraft({ remoteHarnessId: null })),
    false,
  );
});

test("a harness id with no matching catalog entry does not unblock submit", () => {
  assert.equal(
    canSubmitWhereToRun(providerDraft({ remoteHarnessId: "codex" })),
    false,
  );
  assert.equal(
    selectedRemoteHarness(providerDraft({ remoteHarnessId: "codex" })),
    null,
  );
});

test("local drafts never carry a remote harness", () => {
  assert.equal(
    selectedRemoteHarness({ ...providerDraft(), runOn: "local" }),
    null,
  );
});
