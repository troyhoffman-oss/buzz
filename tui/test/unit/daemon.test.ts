/**
 * T0 unit tests for the daemon-handshake half of the client — DESIGN.md §2.3,
 * §2.5, §2.6.
 */

import { describe, expect, test } from "bun:test";
import {
  type ConnectionState,
  type Health,
  MIN_API_VERSION,
  checkApiVersion,
  hasCapability,
  isLossState,
} from "../../src/client/daemon";

const health = (over: Partial<Health> = {}): Health => ({
  version: "0.1.0",
  api_version: MIN_API_VERSION,
  capabilities: ["channels", "agents"],
  archiving: true,
  ...over,
});

describe("api version floor (§2.3 [D-1])", () => {
  test("a newer daemon is accepted — the rule is a floor, not equality", () => {
    // Exact-match would mean "restart the daemon to match your client", i.e.
    // killing the always-on observer archive §6.5's VPS install exists for.
    expect(
      checkApiVersion(health({ api_version: MIN_API_VERSION + 5 })).ok,
    ).toBe(true);
  });

  test("an equal daemon is accepted", () => {
    expect(checkApiVersion(health()).ok).toBe(true);
  });

  test("an older daemon fails with both numbers and the remedy", () => {
    const result = checkApiVersion(
      health({ version: "0.4.1", api_version: MIN_API_VERSION - 1 }),
    );
    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("unreachable");
    expect(result.message).toContain("0.4.1");
    expect(result.message).toContain("upgrade the daemon");
  });
});

describe("capabilities gate which screens exist (§2.3 [D-1])", () => {
  test("an implemented group is present", () => {
    expect(hasCapability(health(), "channels")).toBe(true);
  });

  test("a later-wave group is absent rather than erroring", () => {
    // §2.4: everything else "lands in the wave that needs it, and is absent
    // from capabilities[] until then".
    expect(hasCapability(health(), "projects")).toBe(false);
    expect(hasCapability(health(), "moderation")).toBe(false);
  });
});

describe("keyless is a visible state (§2.5)", () => {
  test("archiving false is representable and distinct", () => {
    expect(health({ archiving: false }).archiving).toBe(false);
    expect(health().archiving).toBe(true);
  });
});

describe("connection states (§2.6)", () => {
  test("every non-connected state is a loss state the chrome must surface", () => {
    const states: ConnectionState[] = [
      { state: "disconnected" },
      { state: "connecting" },
      { state: "authenticating" },
      { state: "rate_limited", retry_after_ms: 4000 },
      { state: "reconnecting", attempt: 2, next_retry_in_ms: 2000 },
      { state: "dns_brownout" },
      { state: "auth_failed", reason: "oa_expired" },
    ];
    for (const state of states) {
      expect(isLossState(state)).toBe(true);
    }
    expect(isLossState({ state: "connected" })).toBe(false);
  });

  test("auth failure carries its reason, so remediation can be inline", () => {
    // §2.6: "a NIP-42 rejection or an expired NIP-OA auth tag says so and
    // offers `:login`, it does not say 'disconnected'".
    const state: ConnectionState = {
      state: "auth_failed",
      reason: "oa_expired",
    };
    expect(state.reason).toBe("oa_expired");
  });

  test("reconnecting carries attempt and countdown, not just a spinner", () => {
    const state: ConnectionState = {
      state: "reconnecting",
      attempt: 3,
      next_retry_in_ms: 4000,
    };
    expect(state.attempt).toBe(3);
    expect(state.next_retry_in_ms).toBe(4000);
  });
});
