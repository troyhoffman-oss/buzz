import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  backendProviderLabel,
  backendProviderLabels,
} from "./backendProviderLabel.ts";

describe("backendProviderLabel", () => {
  it("prefers a probed name over the binary-derived id", () => {
    assert.equal(backendProviderLabel("ssh", "SSH"), "SSH");
  });

  it("falls back to the id when no probe has been paid for", () => {
    assert.equal(backendProviderLabel("ssh"), "ssh");
    assert.equal(backendProviderLabel("ssh", null), "ssh");
  });

  it("treats a blank probed name as no name — the id is a better label", () => {
    assert.equal(backendProviderLabel("ssh", "   "), "ssh");
  });

  it("trims a probed name", () => {
    assert.equal(backendProviderLabel("ssh", "  SSH  "), "SSH");
  });

  it("never renders an empty label", () => {
    assert.equal(backendProviderLabel("  "), "Unknown provider");
  });
});

describe("backendProviderLabels", () => {
  it("returns an empty list when nothing is discovered", () => {
    assert.deepEqual(backendProviderLabels([]), []);
  });

  it("sorts so a PATH-order change cannot reshuffle the line", () => {
    const providers = [
      { id: "ssh", binaryPath: "/usr/bin/buzz-backend-ssh" },
      { id: "blox", binaryPath: "/usr/bin/buzz-backend-blox" },
    ];
    assert.deepEqual(backendProviderLabels(providers), ["blox", "ssh"]);
    assert.deepEqual(backendProviderLabels([...providers].reverse()), [
      "blox",
      "ssh",
    ]);
  });

  it("uses probed names where they exist and ids elsewhere", () => {
    const providers = [
      { id: "ssh", binaryPath: "/usr/bin/buzz-backend-ssh" },
      { id: "blox", binaryPath: "/usr/bin/buzz-backend-blox" },
    ];
    // `localeCompare`, so the order is the human one ("blox" before "SSH")
    // rather than the code-point one that would strand every capitalized
    // provider name ahead of every lowercase id.
    assert.deepEqual(backendProviderLabels(providers, { ssh: "SSH" }), [
      "blox",
      "SSH",
    ]);
  });
});
