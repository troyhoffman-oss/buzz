import assert from "node:assert/strict";
import test from "node:test";

import {
  createGateHarnessId,
  createRuntimeIsAvailable,
  createRuntimeOptionDisabled,
  createRuntimeSelectionSatisfied,
  runtimeDropdownOptions,
  runtimeDropdownPlaceholder,
} from "./createRuntimeGate.ts";
import { requiredCredentialEnvKeys } from "./agentConfigOptions.tsx";

function runtimeEntry(overrides = {}) {
  return {
    id: "buzz-agent",
    label: "Buzz Agent",
    availability: "available",
    requiresExternalCli: false,
    installHint: "",
    ...overrides,
  };
}

function gate(overrides = {}) {
  return {
    isCreateMode: true,
    runsRemotely: false,
    runtime: "buzz-agent",
    selectedRuntime: runtimeEntry(),
    hasLocalDefaultRuntime: true,
    ...overrides,
  };
}

test("a local create requires a locally-available runtime", () => {
  assert.equal(createRuntimeSelectionSatisfied(gate()), true);
  assert.equal(
    createRuntimeSelectionSatisfied(
      gate({
        selectedRuntime: runtimeEntry({ availability: "not_installed" }),
      }),
    ),
    false,
  );
  assert.equal(
    createRuntimeSelectionSatisfied(gate({ runtime: "  " })),
    false,
    "a local create must name a runtime",
  );
});

test("a remote create is not gated by the local catalog", () => {
  const remote = gate({
    runsRemotely: true,
    runtime: "",
    selectedRuntime: null,
  });
  assert.equal(createRuntimeSelectionSatisfied(remote), true);
  assert.equal(createRuntimeIsAvailable(remote), true);
  assert.equal(
    createRuntimeIsAvailable(
      gate({
        runsRemotely: true,
        selectedRuntime: runtimeEntry({ availability: "not_installed" }),
      }),
    ),
    true,
  );
});

test("edit mode never applies the create-only runtime requirement", () => {
  assert.equal(
    createRuntimeSelectionSatisfied(
      gate({ isCreateMode: false, runtime: "", selectedRuntime: null }),
    ),
    true,
  );
});

test("unavailable options are disabled only for a gated local create", () => {
  const missing = runtimeEntry({ id: "goose", availability: "not_installed" });
  assert.equal(createRuntimeOptionDisabled(missing, gate()), true);
  assert.equal(
    createRuntimeOptionDisabled(missing, gate({ runsRemotely: true })),
    false,
    "the remote host's catalog decides availability, not this machine's",
  );
  assert.equal(
    createRuntimeOptionDisabled(missing, gate({ isCreateMode: false })),
    false,
  );
  assert.equal(
    createRuntimeOptionDisabled(
      missing,
      gate({ hasLocalDefaultRuntime: false }),
    ),
    false,
    "with nothing installed, disabling every option would trap the user",
  );
});

test("the dropdown offers a blank option only outside create mode", () => {
  const runtimes = [runtimeEntry()];
  const created = runtimeDropdownOptions({
    defaultRuntimeId: "buzz-agent",
    gate: gate(),
    runtimes,
    runtimesLoading: false,
  });
  assert.deepEqual(
    created.map((option) => option.value),
    ["buzz-agent"],
  );
  assert.equal(created[0].label, "Buzz Agent (default)");

  const edited = runtimeDropdownOptions({
    defaultRuntimeId: "buzz-agent",
    gate: gate({ isCreateMode: false }),
    runtimes,
    runtimesLoading: false,
  });
  assert.deepEqual(
    edited.map((option) => option.value),
    ["__no_runtime__", "buzz-agent"],
  );
  assert.equal(edited[1].label, "Buzz Agent", "no default marker when editing");
});

test("a runtime the catalog no longer knows keeps its own entry", () => {
  const options = runtimeDropdownOptions({
    defaultRuntimeId: "buzz-agent",
    gate: gate({ isCreateMode: false, runtime: "retired-harness" }),
    runtimes: [runtimeEntry()],
    runtimesLoading: false,
  });
  assert.deepEqual(options.at(-1), {
    label: "retired-harness (current)",
    value: "retired-harness",
  });
});

test("remote dropdown options are all selectable", () => {
  const options = runtimeDropdownOptions({
    defaultRuntimeId: "buzz-agent",
    gate: gate({ runsRemotely: true }),
    runtimes: [runtimeEntry({ id: "goose", availability: "not_installed" })],
    runtimesLoading: false,
  });
  assert.equal(options[0].disabled, false);
});

test("the placeholder tracks loading and mode", () => {
  assert.equal(
    runtimeDropdownPlaceholder({ isCreateMode: true, runtimesLoading: true }),
    "Loading harnesses...",
  );
  assert.equal(
    runtimeDropdownPlaceholder({ isCreateMode: true, runtimesLoading: false }),
    "Choose a harness",
  );
  assert.equal(
    runtimeDropdownPlaceholder({ isCreateMode: false, runtimesLoading: false }),
    "No preference (use app default)",
  );
});

test("a local create asks the local runtime for its credential keys", () => {
  assert.equal(
    createGateHarnessId({
      runsRemotely: false,
      runtime: "buzz-agent",
      remoteHarnessId: "goose",
    }),
    "buzz-agent",
    "a stale remote pin never leaks into a local create",
  );
});

test("a remote goose on a buzz-agent laptop demands GOOSE_*, not BUZZ_AGENT_*", () => {
  // The bug this guards: `runtime` is seeded from the LOCAL catalog, so a
  // machine defaulting to buzz-agent would demand BUZZ_AGENT-shaped
  // credentials for an agent that runs Goose on someone else's host.
  const harnessId = createGateHarnessId({
    runsRemotely: true,
    runtime: "buzz-agent",
    remoteHarnessId: "goose",
  });
  assert.equal(harnessId, "goose");
  assert.deepEqual(requiredCredentialEnvKeys(harnessId, "anthropic"), [
    "ANTHROPIC_API_KEY",
  ]);
});

test("a remote goose on a claude laptop still demands credentials", () => {
  // The other half of the same bug: claude/codex support no provider
  // selection, so the local id made the requirement list empty and the create
  // shipped with no provider, model, or key at all.
  assert.deepEqual(requiredCredentialEnvKeys("claude", "anthropic"), []);
  assert.deepEqual(
    requiredCredentialEnvKeys(
      createGateHarnessId({
        runsRemotely: true,
        runtime: "claude",
        remoteHarnessId: "goose",
      }),
      "anthropic",
    ),
    ["ANTHROPIC_API_KEY"],
  );
});

test("an unpinned remote harness demands nothing yet", () => {
  const harnessId = createGateHarnessId({
    runsRemotely: true,
    runtime: "buzz-agent",
    remoteHarnessId: null,
  });
  assert.equal(harnessId, "");
  assert.deepEqual(requiredCredentialEnvKeys(harnessId, "anthropic"), []);
});
