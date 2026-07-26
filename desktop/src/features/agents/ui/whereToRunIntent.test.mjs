import assert from "node:assert/strict";
import test from "node:test";

import {
  canSubmitWhereToRun,
  emptyWhereToRunDraft,
  providerConfigComplete,
  remoteModelDiscoveryView,
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

function modelsResponse(overrides = {}) {
  return {
    agentName: "Goose",
    agentVersion: "1.2.0",
    models: [{ id: "gpt-5", name: "GPT-5" }],
    agentDefaultModel: "gpt-5",
    selectedModel: null,
    supportsSwitching: true,
    ...overrides,
  };
}

// The whole point of the remote probe: a local draft, or one without a picked
// harness, has nothing to have probed, so the local discovery path keeps
// owning the Model control.
test("model discovery view is null without a picked remote harness", () => {
  assert.equal(remoteModelDiscoveryView(emptyWhereToRunDraft), null);
  assert.equal(
    remoteModelDiscoveryView(providerDraft({ remoteHarnessId: null })),
    null,
  );
  assert.equal(
    remoteModelDiscoveryView({ ...providerDraft(), runOn: "local" }),
    null,
  );
});

test("an unprobed harness leaves the model control to the local path", () => {
  assert.equal(
    remoteModelDiscoveryView(
      providerDraft({ remoteModelProbe: { status: "idle" } }),
    ),
    null,
  );
});

test("an in-flight probe reports loading with no options and no status", () => {
  assert.deepEqual(
    remoteModelDiscoveryView(
      providerDraft({ remoteModelProbe: { status: "loading" } }),
    ),
    {
      harnessId: "goose",
      discoveredModelOptions: null,
      modelDiscoveryLoading: true,
      modelDiscoveryStatus: null,
    },
  );
});

test("a loaded probe offers the host's models plus a default row", () => {
  const view = remoteModelDiscoveryView(
    providerDraft({
      remoteModelProbe: { status: "loaded", models: modelsResponse() },
    }),
  );
  assert.equal(view.harnessId, "goose");
  assert.equal(view.modelDiscoveryLoading, false);
  assert.equal(view.modelDiscoveryStatus, null);
  assert.deepEqual(view.discoveredModelOptions, [
    { id: "", label: "Default model (gpt-5)" },
    { id: "gpt-5", label: "GPT-5" },
  ]);
});

// A failed probe must not fall back to this computer's catalog: it would
// scope the picker to models the remote harness cannot run.
test("a failed probe surfaces host-specific copy and no options", () => {
  const view = remoteModelDiscoveryView(
    providerDraft({
      remoteModelProbe: { status: "failed", error: "ssh: connection refused" },
    }),
  );
  assert.equal(view.discoveredModelOptions, null);
  assert.equal(view.modelDiscoveryLoading, false);
  assert.equal(view.modelDiscoveryStatus.tone, "warning");
  assert.match(view.modelDiscoveryStatus.message, /ssh: connection refused/);
});

test("a harness that reports no models warns about the host, not this machine", () => {
  const view = remoteModelDiscoveryView(
    providerDraft({
      remoteModelProbe: {
        status: "loaded",
        models: modelsResponse({ models: [], agentDefaultModel: null }),
      },
    }),
  );
  assert.equal(view.discoveredModelOptions, null);
  assert.equal(view.modelDiscoveryStatus.tone, "warning");
  assert.match(view.modelDiscoveryStatus.message, /Goose reported no models/);
  assert.match(view.modelDiscoveryStatus.message, /on the host/);
});
