/**
 * Fixture replay — the §5.4 half that shipped without a driver.
 *
 * A scenario is "an initial state snapshot, **then the event stream**". The
 * transport implemented both halves and the product drove only the first, so
 * `BUZZ_TUI_FIXTURE` rendered line 1 of every scenario and stopped. Nothing
 * caught it: the render suite calls `advanceTo` itself (`test/helpers/drive.ts`)
 * and therefore proved the transport rather than the product, and a frozen
 * scenario still paints a full, plausible screen — §1.3 property 3's failure
 * mode exactly, reached through the one seam nothing exercised.
 *
 * So the first test below is the regression test for "the product advances the
 * stream at all", and it is deliberately written against the arithmetic rather
 * than against a timer: a test that slept would be the flaky-by-construction
 * shape `test/tmux/README.md` names as the number-one source of flaky TUI CI.
 */

import { describe, expect, test } from "bun:test";
import { FixtureClient } from "../../src/client/fixture-client";
import {
  replayUntilFromEnv,
  startFixtureReplay,
} from "../../src/client/fixture-replay";

/** A hand-held clock, so every case below is exact and instant. */
function fakeClock(): { now: () => number; set: (ms: number) => void } {
  let t = 1_000_000;
  return {
    now: () => t,
    set: (ms: number) => {
      t = 1_000_000 + ms;
    },
  };
}

describe("the scenario's event stream reaches the app (§5.4)", () => {
  test("a connection.state frame changes the rendered snapshot", () => {
    const client = FixtureClient.fromFile("reconnect");
    const clock = fakeClock();
    const replay = startFixtureReplay(client, { now: clock.now });

    // Time zero is the boot instant, so nothing past `atMs: 0` has arrived.
    expect(client.getSnapshot().session.connection.state).toBe("connected");

    clock.set(600);
    replay.tick();
    const degraded = client.getSnapshot().session.connection;
    expect(degraded.state).toBe("reconnecting");
    // The attempt number is the payload the statusline renders as `◌ retry 2`;
    // asserting the state alone would pass on a frame that lost its fields.
    expect(degraded).toMatchObject({ state: "reconnecting", attempt: 2 });

    clock.set(2100);
    replay.tick();
    expect(client.getSnapshot().session.connection.state).toBe("connected");
  });

  test("a tick delivers every frame due, not merely the next one", () => {
    // A driver that advanced one line per tick would replay a 200-event burst
    // over ten seconds. `advanceTo` is a watermark, and this pins that.
    const client = FixtureClient.fromFile("reconnect");
    const clock = fakeClock();
    const replay = startFixtureReplay(client, { now: clock.now });

    clock.set(5_000);
    expect(replay.tick()).toBe(true);
    expect(client.isDrained()).toBe(true);
    expect(client.getSnapshot().session.connection.state).toBe("connected");
  });

  test("a drained scenario reports completion so the interval can stop", () => {
    const client = FixtureClient.fromFile("seeded-basic");
    const clock = fakeClock();
    const replay = startFixtureReplay(client, { now: clock.now });
    // A snapshot-only scenario has nothing left after construction, so the
    // first tick is also the last. Without this the shipped binary would wake
    // twenty times a second forever on every single-line fixture.
    expect(replay.tick()).toBe(true);
  });

  test("a clock that steps backwards stalls nothing", () => {
    // `advanceTo` only moves forward, so a rewound clock must clamp rather than
    // hand it a smaller watermark — under which the stream would silently stop.
    const client = FixtureClient.fromFile("reconnect");
    let t = 1_000_000;
    const replay = startFixtureReplay(client, { now: () => t });

    t = 1_000_000 - 5_000;
    replay.tick();
    expect(client.getSnapshot().session.connection.state).toBe("connected");

    t = 1_000_000 + 600;
    replay.tick();
    expect(client.getSnapshot().session.connection.state).toBe("reconnecting");
  });
});

describe("BUZZ_TUI_FIXTURE_UNTIL freezes a transient state", () => {
  test("the scenario stops at the bound instead of recovering", () => {
    // `reconnect` is degraded for 1.5 s and then heals. A capture harness that
    // waits for the frame to settle can only ever record the healed frame —
    // which is why the Wave 2 capture set shipped without this state at all.
    const client = FixtureClient.fromFile("reconnect");
    const clock = fakeClock();
    const replay = startFixtureReplay(client, { now: clock.now, until: 1_000 });

    clock.set(600);
    // Not finished: the bound has not been reached, so frames between here and
    // 1000 ms are still owed. Reporting completion early would stop the
    // interval on a scenario that still had events to deliver.
    expect(replay.tick()).toBe(false);
    expect(client.getSnapshot().session.connection.state).toBe("reconnecting");

    // Well past the fixture's own recovery at 2000 ms: the bound holds, so the
    // degraded frame is a resting state rather than a window to race.
    clock.set(60_000);
    expect(replay.tick()).toBe(true);
    expect(client.getSnapshot().session.connection.state).toBe("reconnecting");
    expect(client.isDrained()).toBe(false);
  });

  test("a bound past the scenario's end still plays it out", () => {
    const client = FixtureClient.fromFile("reconnect");
    const clock = fakeClock();
    const replay = startFixtureReplay(client, {
      now: clock.now,
      until: 90_000,
    });
    clock.set(90_000);
    expect(replay.tick()).toBe(true);
    expect(client.getSnapshot().session.connection.state).toBe("connected");
  });

  test("the env value is parsed, and a malformed one is refused", () => {
    expect(replayUntilFromEnv({})).toBeUndefined();
    expect(replayUntilFromEnv({ BUZZ_TUI_FIXTURE_UNTIL: "1000" })).toBe(1000);
    expect(replayUntilFromEnv({ BUZZ_TUI_FIXTURE_UNTIL: "0" })).toBe(0);
    // Silently ignoring these would let a harness believe it had pinned a
    // transient while the scenario ran straight past it — a wrong artifact that
    // looks right, which is the whole failure this module was written to end.
    expect(() =>
      replayUntilFromEnv({ BUZZ_TUI_FIXTURE_UNTIL: "soon" }),
    ).toThrow();
    expect(() =>
      replayUntilFromEnv({ BUZZ_TUI_FIXTURE_UNTIL: "-1" }),
    ).toThrow();
  });
});
