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
 * Compose `text` in the mentioned channel and press `⏎`.
 *
 * `→` from boot teleports into the channel of the top mention (§4.4), which is
 * the shortest route to a layer whose composer posts.
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
  // The send is async and the key handler does not await it, so give the
  // microtask queue a turn before asserting on what the client saw.
  await Bun.sleep(20);
  await t.renderOnce();
}

test("the Shell calls client.send when ⏎ is pressed with composer text", async () => {
  const { client, t } = await mount();

  await composeAndSend(t, "ship it");

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

  expect(client.sends).toHaveLength(1);
  // A cleared composer plus a message that never arrived is indistinguishable
  // from the bug this seam was added to fix. The operator keeps their text.
  expect(t.captureCharFrame()).toContain("will fail");
});
