/**
 * The Shell's effect drain, mounted for real — the seam between the pure
 * reducer and the daemon.
 *
 * The T1 walkthroughs drive `applyIntent` directly and the T2 tmux suite
 * drives a whole process; neither mounts `Shell` itself, which is precisely
 * where the write path was dead. These cases sit in that gap: a real component
 * with a stub client, so what the Shell *calls* is observable.
 */

import { expect, test } from "bun:test";
import { testRender } from "@opentui/solid";
import type {
  DaemonClient,
  StreamListener,
} from "../../src/client/daemon-client";
import { FixtureClient } from "../../src/client/fixture-client";
import type { Message } from "../../src/client/types";
import { Shell } from "../../src/shell/Shell";
import { FIXED_NOW } from "../helpers/drive";

/** One recorded `send` call. */
interface SendCall {
  channelId: string;
  content: string;
  options?: { replyTo?: string; mentions?: readonly string[] };
}

/**
 * A client that records what it was asked to do, wrapping a real fixture so
 * the screens have something to render.
 *
 * `sendFails` makes the daemon reject — the case that decides whether a failed
 * send is silent, which is the failure mode this whole seam exists to prevent.
 */
class RecordingClient implements DaemonClient {
  readonly sends: SendCall[] = [];
  readonly readMarks: string[] = [];
  readonly messageLoads: string[] = [];

  constructor(
    private readonly inner: FixtureClient,
    private readonly sendFails = false,
  ) {}

  getSnapshot() {
    return this.inner.getSnapshot();
  }

  subscribe(listener: StreamListener): () => void {
    return this.inner.subscribe(listener);
  }

  async send(
    channelId: string,
    content: string,
    options?: { replyTo?: string; mentions?: readonly string[] },
  ): Promise<Message> {
    this.sends.push({ channelId, content, ...(options ? { options } : {}) });
    if (this.sendFails) throw new Error("relay_unreachable");
    return this.inner.send(channelId, content, options);
  }

  async markRead(channelId: string, eventId?: string): Promise<void> {
    this.readMarks.push(channelId);
    return this.inner.markRead(channelId, eventId);
  }

  /**
   * Recorded as well as delegated, so a test can assert the Shell asks for a
   * timeline when a channel layer becomes current — the effect that carries the
   * whole L2 body on the socket transport, and that renders as an empty
   * (indistinguishable from empty-channel) timeline when it does not fire.
   */
  async ensureMessages(channelId: string): Promise<void> {
    this.messageLoads.push(channelId);
    return this.inner.ensureMessages(channelId);
  }

  messagesGeneration(): number {
    return this.inner.messagesGeneration();
  }
}

/** Mount the Shell with a recording client and return both. */
async function mount(sendFails = false) {
  const client = new RecordingClient(
    FixtureClient.fromFile("seeded-basic"),
    sendFails,
  );
  const t = await testRender(
    () => Shell({ client, now: () => FIXED_NOW, onQuit: () => {} }),
    { width: 120, height: 30 },
  );
  await t.renderOnce();
  return { client, t };
}

/**
 * Drive renders until `predicate` holds, or fail naming what was awaited.
 *
 * The Shell's effects are async and the key handler deliberately does not await
 * them (a rethrow from a key handler is an unhandled rejection, which under
 * Bun can take the process down — see `Shell.tsx`'s `perform`). So every
 * assertion in this file has to wait for a drain that publishes no completion
 * signal.
 *
 * A **fixed** `Bun.sleep(20)` is a guess about scheduler latency, and the guess
 * is false on a loaded machine: at load 50+ on 16 cores the microtask that
 * records the call has simply not run yet, so the suite goes red against a
 * product that is green. That is the worst class of test failure — it costs a
 * bisect to learn there was never a defect, and it trains the next author to
 * dismiss a real one.
 *
 * Polling the condition with a deadline is the same assertion minus the
 * assumption. It returns as fast as the 20 ms version ever did — the first
 * iteration usually holds — and it survives the slow case instead of
 * fabricating a regression from it.
 */
async function settleUntil(
  t: Awaited<ReturnType<typeof mount>>["t"],
  predicate: () => boolean,
  what: string,
  timeoutMs = 15_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    await t.renderOnce();
    if (predicate()) return;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await Bun.sleep(5);
  }
}

/**
 * Compose `text` in the mentioned channel and press `⏎`.
 *
 * `→` from boot teleports into the channel of the top mention (§4.4), which is
 * the shortest route to a layer whose composer posts. Returns with the key
 * pressed and **nothing awaited**: which drained effect the test is waiting on
 * is the test's business, and it says so through {@link settleUntil}.
 */
async function composeAndSend(
  t: Awaited<ReturnType<typeof mount>>["t"],
  text: string,
) {
  t.mockInput.pressArrow("right");
  await t.renderOnce();
  await t.mockInput.typeText(text);
  await t.renderOnce();
  t.mockInput.pressEnter();
}

test("the Shell calls client.send when ⏎ is pressed with composer text", async () => {
  const { client, t } = await mount();

  await composeAndSend(t, "ship it");
  await settleUntil(t, () => client.sends.length > 0, "client.send");

  // The assertion is on the *call*, not on `state.pending`. A reducer test
  // cannot distinguish "decided to send" from "sent", and that is exactly the
  // distinction that was broken.
  expect(client.sends).toHaveLength(1);
  expect(client.sends[0]?.content).toBe("ship it");
  expect(client.sends[0]?.channelId).toBeTruthy();
});

test("a rejected send restores the text rather than losing it silently", async () => {
  const { client, t } = await mount(true);

  await composeAndSend(t, "will fail");
  // Two drains deep: the send must be *attempted*, and the rejection must have
  // been caught and the text written back. Waiting on the restored frame rather
  // than on the call is what makes this the assertion it claims to be.
  await settleUntil(
    t,
    () => t.captureCharFrame().includes("will fail"),
    "the rejected text to be restored",
  );

  expect(client.sends).toHaveLength(1);
  // A cleared composer plus a message that never arrived is indistinguishable
  // from the bug this seam was added to fix. The operator keeps their text.
  expect(t.captureCharFrame()).toContain("will fail");
});

/**
 * **M3 regression.** The L2 body is loaded by an effect, not by the boot
 * snapshot, and nothing above the client can tell "not asked for" from "empty
 * channel" — both paint the same blank rows.
 *
 * Found against the live daemon: `UdsClient.hydrate` hardcoded `messages: {}`
 * with a comment saying the endpoint was unmounted. It is mounted now, so the
 * comment was stale and the timeline rendered 28 blank rows while
 * `GET /channel/{id}/message` returned real messages over the same socket.
 *
 * Every fixture carries its own messages, so no fixture test could see this:
 * `FixtureClient.getSnapshot` already had them, and the missing fetch was
 * invisible. The assertion is therefore on the **call**, which is the thing
 * that was absent.
 */
test("descending into a channel asks the client for its timeline", async () => {
  const { client, t } = await mount();

  t.mockInput.pressArrow("right");
  await settleUntil(
    t,
    () => client.messageLoads.length > 0,
    "client.ensureMessages",
  );

  expect(client.messageLoads.length).toBeGreaterThan(0);
  expect(client.messageLoads[0]).toBeTruthy();
});
