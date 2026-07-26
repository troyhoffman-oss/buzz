import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  remoteServerEntries,
  remoteServerVersionLabel,
} from "./remoteServerGalleryLogic.ts";

function provider(id) {
  return { id, binaryPath: `/home/u/.local/bin/buzz-backend-${id}` };
}

function okProbe(overrides = {}) {
  return {
    status: "ok",
    result: { ok: true, name: "SSH", version: "0.4.26", ...overrides },
  };
}

describe("remoteServerEntries", () => {
  it("returns nothing when no provider is installed", () => {
    assert.deepEqual(remoteServerEntries([], {}), []);
  });

  it("reads name, version and description off a successful probe", () => {
    const entries = remoteServerEntries([provider("ssh")], {
      ssh: okProbe({ description: "Run agents over SSH." }),
    });
    assert.equal(entries.length, 1);
    assert.equal(entries[0].label, "SSH");
    assert.equal(entries[0].version, "0.4.26");
    assert.equal(entries[0].description, "Run agents over SSH.");
    assert.equal(entries[0].status, "ready");
    assert.equal(entries[0].error, null);
    assert.equal(entries[0].binaryPath, "/home/u/.local/bin/buzz-backend-ssh");
  });

  it("falls back to the id while the probe is still in flight", () => {
    const entries = remoteServerEntries([provider("ssh")], {
      ssh: { status: "loading" },
    });
    assert.equal(entries[0].label, "ssh");
    assert.equal(entries[0].status, "probing");
    assert.equal(entries[0].version, null);
  });

  it("treats a missing probe as still in flight", () => {
    const entries = remoteServerEntries([provider("ssh")], {});
    assert.equal(entries[0].status, "probing");
  });

  it("surfaces a spawn failure on the row instead of dropping it", () => {
    const entries = remoteServerEntries([provider("ssh")], {
      ssh: { status: "failed", error: "binary not found" },
    });
    assert.equal(entries[0].status, "unavailable");
    assert.equal(entries[0].error, "binary not found");
    // Still named and listed — a broken provider the user installed is a fact
    // worth showing, not one to hide.
    assert.equal(entries[0].label, "ssh");
  });

  it("treats an ok:false answer as unavailable", () => {
    const entries = remoteServerEntries([provider("ssh")], {
      ssh: { status: "ok", result: { ok: false } },
    });
    assert.equal(entries[0].status, "unavailable");
    assert.equal(
      entries[0].error,
      "The provider did not answer its info request.",
    );
  });

  it("ignores a name carried on an ok:false answer", () => {
    const entries = remoteServerEntries([provider("ssh")], {
      ssh: { status: "ok", result: { ok: false, name: "SSH", version: "9" } },
    });
    assert.equal(entries[0].label, "ssh");
    assert.equal(entries[0].version, null);
  });

  it("blank name/version/description read as absent, not as empty strings", () => {
    const entries = remoteServerEntries([provider("ssh")], {
      ssh: {
        status: "ok",
        result: { ok: true, name: "  ", version: "  ", description: "  " },
      },
    });
    assert.equal(entries[0].label, "ssh");
    assert.equal(entries[0].version, null);
    assert.equal(entries[0].description, null);
  });

  it("sorts ready rows first, then alphabetically, so PATH order cannot reshuffle it", () => {
    const providers = [provider("zeta"), provider("blox"), provider("ssh")];
    const probes = {
      zeta: okProbe({ name: "zeta" }),
      blox: { status: "failed", error: "nope" },
      ssh: okProbe({ name: "SSH" }),
    };
    assert.deepEqual(
      remoteServerEntries(providers, probes).map((entry) => entry.id),
      ["ssh", "zeta", "blox"],
    );
    assert.deepEqual(
      remoteServerEntries([...providers].reverse(), probes).map(
        (entry) => entry.id,
      ),
      ["ssh", "zeta", "blox"],
    );
  });

  it("does not mutate the caller's provider list", () => {
    const providers = [provider("zeta"), provider("blox")];
    const snapshot = providers.map((entry) => entry.id);
    remoteServerEntries(providers, {});
    assert.deepEqual(
      providers.map((entry) => entry.id),
      snapshot,
    );
  });
});

describe("remoteServerVersionLabel", () => {
  it("appends a version when there is one", () => {
    const [entry] = remoteServerEntries([provider("ssh")], {
      ssh: okProbe(),
    });
    assert.equal(remoteServerVersionLabel(entry), "SSH 0.4.26");
  });

  it("is just the label when the provider reports no version", () => {
    const [entry] = remoteServerEntries([provider("ssh")], {
      ssh: okProbe({ version: undefined }),
    });
    assert.equal(remoteServerVersionLabel(entry), "SSH");
  });
});
