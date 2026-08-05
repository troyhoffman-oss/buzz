/**
 * T0 — the UDS transport, against a real socket.
 *
 * The server here is `Bun.serve({ unix })` rather than a mocked `fetch`, for
 * the same reason `crates/buzz-daemon/tests/api.rs` binds a real
 * `UnixListener` rather than calling `Router::oneshot`: every property under
 * test is a property of a *socket*. A mocked fetch cannot show that an ndjson
 * frame split across two writes still decodes, that an abort mid-read is
 * distinguishable from a stream ending, or that a 404 on one endpoint leaves
 * the others working.
 *
 * **No `buzz-daemon` process runs.** The server below replies with bodies
 * copied from `tests/api.rs`, so this suite is hermetic and still asserts
 * against the shapes the daemon really produces.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { UdsClient } from "../../src/client/uds-client";
import { backoffMs } from "../../src/client/uds-client";

/** A fake daemon on a real Unix socket. */
class FakeDaemon {
  readonly socket: string;
  private readonly dir: string;
  private readonly server: ReturnType<typeof Bun.serve>;
  /** Paths that answered, in request order — the assertion target for fan-out. */
  readonly requested: string[] = [];
  /** Lines the `/event` stream will write, in order. */
  streamChunks: string[] = [];
  /** Status `/event` answers with. `404` models the daemon that exists today. */
  streamStatus = 200;
  /** Bodies keyed by path; a missing key is a 404, like an unmounted route. */
  routes: Record<string, unknown> = {};
  /** Paths that should answer 500 rather than 200 or 404. */
  broken = new Set<string>();

  constructor() {
    this.dir = mkdtempSync(join(tmpdir(), "buzz-uds-"));
    this.socket = join(this.dir, "daemon.sock");
    this.server = Bun.serve({
      unix: this.socket,
      fetch: (request) => this.handle(request),
    });
  }

  private handle(request: Request): Response {
    const url = new URL(request.url);
    const path = `${url.pathname}${url.search}`;
    this.requested.push(path);

    if (url.pathname === "/event") {
      if (this.streamStatus !== 200) {
        return Response.json(
          { error: { code: "not_found", message: "no route /event" } },
          { status: this.streamStatus },
        );
      }
      const chunks = this.streamChunks;
      let index = 0;
      const body = new ReadableStream<Uint8Array>({
        async pull(controller) {
          if (index >= chunks.length) {
            controller.close();
            return;
          }
          const chunk = chunks[index++];
          controller.enqueue(new TextEncoder().encode(chunk ?? ""));
          // A real daemon does not emit its whole ring in one syscall; pacing
          // is what makes the partial-line case below reachable at all.
          await Bun.sleep(10);
        },
      });
      return new Response(body, {
        headers: { "content-type": "application/x-ndjson" },
      });
    }

    if (this.broken.has(url.pathname)) {
      return Response.json(
        { error: { code: "internal", message: "boom" } },
        { status: 500 },
      );
    }

    const body = this.routes[url.pathname];
    if (body === undefined) {
      // Shaped exactly like the daemon's own 404 (`error::DaemonError`).
      return Response.json(
        { error: { code: "not_found", message: `no route ${url.pathname}` } },
        { status: 404 },
      );
    }
    return Response.json(body);
  }

  stop(): void {
    this.server.stop(true);
    rmSync(this.dir, { recursive: true, force: true });
  }
}

/** The bodies a Wave-1 daemon serves, copied from `tests/api.rs`. */
function wave1Routes(): Record<string, unknown> {
  return {
    "/health": {
      status: "ok",
      version: "0.1.0",
      api_version: 1,
      capabilities: ["channels", "agents"],
      archiving: true,
      uptime_secs: 4,
    },
    "/session": {
      pubkey: "aa".repeat(32),
      relay_url: "wss://relay.example",
      auth_tag_owner: null,
      connection: { state: "connected" },
      archiving: true,
    },
    "/channel": {
      channels: [
        {
          id: "11111111-1111-1111-1111-111111111111",
          name: "engineering",
          topic: "relay + desktop",
          channel_type: "channel",
          member_count: 2,
          unread: 4,
          mentions: 2,
          agents_working: ["claude-1"],
          archived: false,
        },
      ],
      totals: { unread: 4, mentions: 2 },
    },
    "/agent/fleet": {
      agents: [
        {
          pubkey: "bb".repeat(32),
          name: "claude-1",
          state: "working",
          turn: "4a91",
          elapsed_secs: 252,
        },
      ],
    },
    "/agent/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/activity":
      {
        frames: [
          {
            agent_pubkey: "bb".repeat(32),
            seq: 1,
            timestamp: "2026-08-04T14:12:00Z",
            created_at: 1_785_852_720,
            kind: "acp_read",
            payload: {},
          },
        ],
      },
  };
}

let daemon: FakeDaemon | null = null;

afterEach(() => {
  daemon?.stop();
  daemon = null;
});

function start(): FakeDaemon {
  daemon = new FakeDaemon();
  daemon.routes = wave1Routes();
  return daemon;
}

describe("attach handshake (§2.3 [D-1])", () => {
  test("a Wave-1 daemon's snapshot reaches the screens", async () => {
    const fake = start();
    const client = await UdsClient.connect({ socket: fake.socket });
    const snapshot = client.getSnapshot();

    expect(snapshot.session.pubkey).toBe("aa".repeat(32));
    expect(snapshot.session.connection.state).toBe("connected");
    expect(snapshot.channels[0]?.name).toBe("engineering");
    expect(snapshot.channels[0]?.agentsWorking).toEqual(["claude-1"]);
    expect(snapshot.agents[0]?.name).toBe("claude-1");
    expect(snapshot.transcripts[`${"bb".repeat(32)}`]).toHaveLength(1);
    client.close();
  });

  test("a daemon below the API floor is refused with both numbers", async () => {
    const fake = start();
    fake.routes["/health"] = {
      version: "0.4.1",
      api_version: 0,
      capabilities: [],
      archiving: true,
    };
    // §2.3 [D-1]: "below the floor is a hard error". Attaching anyway would
    // produce a screen built on shapes the daemon does not promise.
    expect(UdsClient.connect({ socket: fake.socket })).rejects.toThrow(
      /0\.4\.1/,
    );
  });

  test("an unreachable socket rejects rather than half-attaching", async () => {
    expect(
      UdsClient.connect({ socket: "/tmp/definitely-not-a-buzz-socket" }),
    ).rejects.toThrow();
  });
});

describe("unmounted endpoints degrade one slice and are recorded", () => {
  test("a 404 empties its collection and lands in missing[]", async () => {
    const fake = start();
    // Exactly the daemon that exists today: `/agent/fleet` is mounted but the
    // metric route is not, so this is not a hypothetical.
    delete fake.routes["/agent/fleet"];
    const client = await UdsClient.connect({ socket: fake.socket });

    expect(client.getSnapshot().agents).toEqual([]);
    // §1.3 property 3: "no agents" and "the fleet route is not mounted" must
    // not look the same from outside. This is what makes them different.
    expect(client.missing).toContain("/agent/fleet");
    // …and the endpoints that *are* mounted still worked.
    expect(client.getSnapshot().channels).toHaveLength(1);
    client.close();
  });

  test("a 500 throws — a broken daemon must not look like an empty one", async () => {
    const fake = start();
    fake.broken.add("/channel");
    // A fault the operator needs to see. Treating it like an unmounted route
    // would hide a broken daemon behind an empty screen for as long as it
    // stayed broken.
    expect(UdsClient.connect({ socket: fake.socket })).rejects.toThrow();
  });
});

describe("the event stream (§2.6 link A, [D-5])", () => {
  test("frames reach a subscriber and fold into the snapshot", async () => {
    const fake = start();
    fake.streamChunks = [
      `${JSON.stringify({
        seq: 1,
        type: "message.new",
        payload: {
          id: "ev1",
          channel_id: "11111111-1111-1111-1111-111111111111",
          content: "from the stream",
          created_at: 1_785_852_720,
          author: { pubkey: "cc".repeat(32), name: "matt" },
        },
      })}\n`,
    ];
    const client = await UdsClient.connect({ socket: fake.socket });
    const seen: string[] = [];
    const unsubscribe = client.subscribe((frame) => seen.push(frame.type));

    await Bun.sleep(200);
    expect(seen).toContain("message.new");
    const messages =
      client.getSnapshot().messages["11111111-1111-1111-1111-111111111111"];
    expect(messages?.[0]?.content).toBe("from the stream");

    unsubscribe();
    client.close();
  });

  test("a frame split across two reads is not dropped", async () => {
    // ndjson frames split at arbitrary byte offsets. Parsing per-chunk would
    // drop every frame that straddles a boundary — rare enough to pass a test
    // written against whole lines, and common enough to lose messages live.
    const fake = start();
    const frame = JSON.stringify({
      seq: 7,
      type: "message.new",
      payload: {
        id: "split",
        channel_id: "11111111-1111-1111-1111-111111111111",
        content: "half now, half later",
        created_at: 1_785_852_720,
        author: { pubkey: "cc".repeat(32), name: "matt" },
      },
    });
    const cut = Math.floor(frame.length / 2);
    fake.streamChunks = [frame.slice(0, cut), `${frame.slice(cut)}\n`];

    const client = await UdsClient.connect({ socket: fake.socket });
    const seen: string[] = [];
    client.subscribe((f) => seen.push(f.type));
    await Bun.sleep(250);

    expect(seen).toContain("message.new");
    expect(
      client.getSnapshot().messages["11111111-1111-1111-1111-111111111111"]?.[0]
        ?.content,
    ).toBe("half now, half later");
    client.close();
  });

  test("a replayed message is idempotent by event id", async () => {
    // `?since=` replays from the ring on reconnect, so the same message can
    // arrive twice. Appending blind would double every message across a
    // reconnect — the exact bug the cursor exists to prevent.
    const fake = start();
    const frame = (seq: number) =>
      `${JSON.stringify({
        seq,
        type: "message.new",
        payload: {
          id: "same-event",
          channel_id: "11111111-1111-1111-1111-111111111111",
          content: "once",
          created_at: 1_785_852_720,
          author: { pubkey: "cc".repeat(32), name: "matt" },
        },
      })}\n`;
    fake.streamChunks = [frame(1), frame(2)];

    const client = await UdsClient.connect({ socket: fake.socket });
    client.subscribe(() => {});
    await Bun.sleep(250);

    expect(
      client.getSnapshot().messages["11111111-1111-1111-1111-111111111111"],
    ).toHaveLength(1);
    client.close();
  });

  test("a 404 on /event stops rather than retrying forever", async () => {
    // The route is unmounted on the daemon that exists today (`api.rs`'s
    // `MOUNTED_ENDPOINTS`), so this is the live case rather than a
    // hypothetical. Retrying a 404 on a backoff ladder would be a busy loop
    // against a route that cannot appear without a restart.
    //
    // The assertion is a **count**, not "it did not throw": a retry loop also
    // does not throw, and the whole failure mode here is invisible traffic.
    const fake = start();
    fake.streamStatus = 404;
    const client = await UdsClient.connect({ socket: fake.socket });
    client.subscribe(() => {});
    // Well past the first ladder rung (1 s), so a retrying client would have
    // made a second and third request by now.
    await Bun.sleep(2500);

    const streamRequests = fake.requested.filter((p) => p.startsWith("/event"));
    expect(streamRequests).toHaveLength(1);
    expect(client.missing).toContain("/event");
    client.close();
  });

  test("stream.reset invalidates and re-fetches rather than showing a gap", async () => {
    // §2.6: "If the cursor has aged out, the daemon sends `stream.reset`
    // **first**, and the TUI invalidates and re-fetches rather than presenting
    // a silently gapped timeline."
    const fake = start();
    fake.streamChunks = [`${JSON.stringify({ type: "stream.reset" })}\n`];
    const client = await UdsClient.connect({ socket: fake.socket });
    const before = fake.requested.filter((p) => p === "/channel").length;

    const seen: string[] = [];
    client.subscribe((frame) => seen.push(frame.type));
    await Bun.sleep(250);

    expect(seen).toContain("stream.reset");
    // The re-fetch is the whole behaviour: without it the client keeps a
    // snapshot missing every event between its dead cursor and now, and
    // nothing on screen says so.
    expect(
      fake.requested.filter((p) => p === "/channel").length,
    ).toBeGreaterThan(before);
    client.close();
  });

  /**
   * **M3 regression.** A reset must re-open `ensureMessages`' idempotence gate,
   * and an *ordinary* refresh must not blank the timeline.
   *
   * Two halves of one bug. `hydrate` used to write `messages: {}`, and
   * `refresh()` calls `hydrate` — so a reset emptied the timeline of the
   * channel the operator was standing in, while the Shell's load effect (keyed
   * on the channel id, which did not change) did not re-run. The body went
   * blank and stayed blank until the operator navigated away and back.
   *
   * Fixing that by carrying `messages` across a refresh would have created the
   * opposite bug — a stale page pinned forever behind the idempotence check —
   * so invalidation is now explicit and the generation counter is what lets a
   * consumer keyed on the current channel notice it.
   */
  test("a reset invalidates timelines; an ordinary refresh preserves them", async () => {
    const fake = start();
    const channelId = "11111111-1111-1111-1111-111111111111";
    fake.routes[`/channel/${channelId}/message`] = {
      messages: [
        {
          event: {
            id: "aa".repeat(32),
            pubkey: "bb".repeat(32),
            created_at: 1_700_000_100,
            content: "hello",
          },
          thread: null,
        },
      ],
      aux: [],
      has_more: false,
    };
    const client = await UdsClient.connect({ socket: fake.socket });

    await client.ensureMessages(channelId);
    const loaded = client.getSnapshot().messages[channelId];
    expect(loaded).toBeDefined();
    const generation = client.messagesGeneration();

    // An ordinary re-hydrate must not lose the page: nothing would refill it.
    await client.refresh();
    expect(client.getSnapshot().messages[channelId]).toBeDefined();
    expect(client.messagesGeneration()).toBe(generation);

    // An invalidation drops it *and* moves the generation, which is the signal
    // "same channel, fetch it again".
    client.invalidateMessages();
    expect(client.getSnapshot().messages[channelId]).toBeUndefined();
    expect(client.messagesGeneration()).toBeGreaterThan(generation);

    // And the gate is genuinely re-opened.
    const requestsBefore = fake.requested.filter((p) =>
      p.includes("/message"),
    ).length;
    await client.ensureMessages(channelId);
    expect(
      fake.requested.filter((p) => p.includes("/message")).length,
    ).toBeGreaterThan(requestsBefore);

    client.close();
  });

  test("resubscribing does not leave a second read loop running", async () => {
    // Found in self-review, confirmed by counting connections before the fix.
    //
    // `readStream` reconnects on the §2.6 ladder, so a loop stopped while it
    // was *asleep* on that ladder used to wake up and reconnect anyway — even
    // though `subscribe` had already started a fresh loop. Both then held an
    // `/event` connection, both wrote `this.cursor`, and every frame reached
    // every listener **twice**.
    //
    // The assertion is a connection count rather than a duplicate-delivery
    // check because the doubling is downstream of the leak and only shows up
    // when a frame happens to arrive during the overlap — the leak itself is
    // always there. Measured: 6 connections before, 4 after (one loop's own
    // ladder), against a server that closes the stream immediately.
    const fake = start();
    // Empty chunk list: the stream closes at once, which is what drives the
    // reconnect ladder and therefore the whole race.
    fake.streamChunks = [];
    const client = await UdsClient.connect({ socket: fake.socket });

    const unsubscribe = client.subscribe(() => {});
    await Bun.sleep(50);
    unsubscribe();
    client.subscribe(() => {});
    await Bun.sleep(3000);

    const streamRequests = fake.requested.filter((p) => p.startsWith("/event"));
    // One live loop over ~3 s reaches rung 3 at most (1 s + 2 s). Two loops
    // reach six or more, which is what the pre-fix run measured.
    expect(streamRequests.length).toBeLessThanOrEqual(4);
    client.close();
  }, 20_000);

  test("unsubscribing the last listener closes the read", async () => {
    const fake = start();
    // A stream that never ends, so the only way the read stops is the abort.
    fake.streamChunks = Array.from(
      { length: 500 },
      (_, i) => `${JSON.stringify({ seq: i + 1, type: "presence.update" })}\n`,
    );
    const client = await UdsClient.connect({ socket: fake.socket });
    let count = 0;
    const unsubscribe = client.subscribe(() => count++);
    await Bun.sleep(120);
    const atUnsubscribe = count;
    unsubscribe();
    await Bun.sleep(200);

    expect(atUnsubscribe).toBeGreaterThan(0);
    // Allow one in-flight frame; what must not happen is the stream running on.
    expect(count - atUnsubscribe).toBeLessThanOrEqual(1);
    client.close();
  });
});

describe("the reconnect ladder (§2.6)", () => {
  test("exponential from 1 s, capped at 30 s", () => {
    expect(backoffMs(1)).toBe(1000);
    expect(backoffMs(2)).toBe(2000);
    expect(backoffMs(5)).toBe(16_000);
    // The cap matters more than the shape: an uncapped ladder reaches hours,
    // and a daemon that came back an hour ago is a TUI that looks hung.
    expect(backoffMs(12)).toBe(30_000);
    expect(backoffMs(99)).toBe(30_000);
  });
});

describe("writes", () => {
  test("send posts to the channel's message endpoint", async () => {
    const fake = start();
    fake.routes["/channel/11111111-1111-1111-1111-111111111111/message"] = {
      event_id: "ev-sent",
      accepted: true,
      message: "",
    };
    const client = await UdsClient.connect({ socket: fake.socket });
    const message = await client.send(
      "11111111-1111-1111-1111-111111111111",
      "hello",
      { mentions: ["dd".repeat(32)] },
    );
    expect(message.id).toBe("ev-sent");
    expect(message.mentions).toEqual(["dd".repeat(32)]);
    client.close();
  });

  test("a rejected send throws with the daemon's stable code, not its message", async () => {
    // §2.4: "the front end switches on `code` and never on a message."
    const fake = start();
    fake.broken.add("/channel/x/message");
    const client = await UdsClient.connect({ socket: fake.socket });
    try {
      await client.send("x", "hello");
      throw new Error("expected the send to reject");
    } catch (error) {
      expect((error as { code?: string }).code).toBe("internal");
    }
    client.close();
  });
});
