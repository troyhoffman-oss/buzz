/**
 * The real transport — HTTP/1.1 + ndjson over the daemon's Unix domain socket
 * (DESIGN.md §2.1, §2.4, `daemon-api.md` Part 2/Part 4).
 *
 * > The invariant that makes the front end disposable: the TUI's entire
 * > network layer is **one HTTP client and one line reader**.
 *
 * That is literally this file. `fetch(url, { unix })` is the HTTP client;
 * {@link UdsClient.subscribe} is the line reader. There is no second socket, no
 * per-channel connection, and no long-poll fallback — which is the property
 * that keeps {@link DaemonClient} at four methods.
 *
 * # Missing endpoints degrade the snapshot; they do not fail the boot
 *
 * The committed daemon mounts a strict subset of `WAVE1_ENDPOINTS`
 * (`crates/buzz-daemon/src/api.rs`'s `MOUNTED_ENDPOINTS`), because the write
 * paths and the timeline reads need the relay session's I/O half. A client
 * that refused to attach until every route existed would be untestable against
 * the daemon that exists, and §2.3 [D-1] already rules on the general case:
 *
 * > At or above the floor, `capabilities[]` decides which screens exist, so a
 * > Wave-3 TUI attached to a Wave-1 daemon on a remote box *hides* what it
 * > cannot serve rather than erroring inside it.
 *
 * So a `404` yields an empty slice of the snapshot **and is recorded** in
 * {@link UdsClient.missing}. Recorded, not swallowed: §1.3 property 3 forbids
 * "no channels" and "the channels endpoint is not mounted" from looking the
 * same, and `buzz-tui doctor` reads that list.
 *
 * # What this file deliberately does not know
 *
 * No relay URL semantics, no key material, no event kinds, no cursor
 * internals. `?since=<seq>` is echoed back exactly as received — [D-6] makes
 * cursors decodable for a human debugging, not for the client — and §6.4's
 * gate fails the build if any of that leaks in.
 */

import { MS_PER_SECOND } from "../time/units";
import { type Health, checkApiVersion } from "./daemon";
import type { DaemonClient, StreamListener } from "./daemon-client";
import type {
  Agent,
  Channel,
  Message,
  Snapshot,
  StreamFrame,
  TranscriptRow,
  Usage,
} from "./types";
import {
  advancesCursor,
  decodeAgent,
  decodeChannel,
  decodeMentionCandidate,
  decodeMessage,
  decodeSession,
  decodeStreamFrame,
  decodeTranscriptRow,
  decodeUsage,
} from "./wire";

/** How the daemon's error bodies are shaped (§2.4/§3.13). */
interface ErrorBody {
  readonly code: string;
  readonly message: string;
}

/** A daemon request that came back non-2xx. */
export class DaemonRequestError extends Error {
  /** HTTP status. */
  readonly status: number;
  /** The daemon's stable `code`, which is what callers switch on — never the
   * message. §2.4: "the front end switches on `code` and never on a message." */
  readonly code: string;

  constructor(status: number, body: ErrorBody) {
    super(body.message);
    this.name = "DaemonRequestError";
    this.status = status;
    this.code = body.code;
  }
}

/** Options for {@link UdsClient.connect}. */
export interface UdsClientOptions {
  /** Absolute path of the daemon's socket. */
  readonly socket: string;
  /** Community label for the statusline, from the TUI's own config. */
  readonly communityName?: string;
  /** Relay URL for the statusline when the daemon has not reported one. */
  readonly relayUrl?: string;
  /** Injected clock — §5.3's determinism requirement reaches the transport too. */
  readonly now?: () => number;
}

/**
 * `fetch` needs an absolute URL even when the transport is a socket path. The
 * host is ignored by the daemon (`daemon-api.md` Part 3: "Host header
 * ignored"), so any authority works; a literal one keeps every request line
 * identical, which matters when reading a packet capture.
 */
const BASE = "http://buzz-daemon";

/**
 * The real daemon client.
 *
 * Construct through {@link UdsClient.connect}, which performs the §2.3
 * handshake (`GET /health` → floor check → first reads) and therefore cannot
 * hand back a client attached to a daemon it cannot talk to.
 */
export class UdsClient implements DaemonClient {
  private readonly socket: string;
  private readonly clock: () => number;
  private readonly communityName: string;
  private readonly relayUrl: string;
  private snapshot: Snapshot;
  private listeners = new Set<StreamListener>();
  /** Newest `seq` seen, handed back as `?since=` on reconnect (§2.6 link A). */
  private cursor = 0;
  /** Aborts the in-flight `/event` read on unsubscribe or teardown. */
  private streamAbort: AbortController | null = null;
  /**
   * Which read loop is the current one.
   *
   * Bumped by {@link stopStream}, so a loop that was asleep on the backoff
   * ladder when it was stopped notices on waking and exits instead of
   * reconnecting. Without it, unsubscribe-then-resubscribe leaves the *old*
   * loop alive alongside the new one: both hold an `/event` connection, both
   * write to `this.cursor`, and every frame is delivered to every listener
   * twice. Measured before the fix — a stream that closes immediately produced
   * six connections where a single loop makes at most three.
   *
   * A plain `streaming: boolean` does not work here, because the loop that must
   * exit is the one *inside* `await sleep(...)`, and by the time it wakes a new
   * loop has already set the flag back to true.
   */
  private streamGeneration = 0;
  private closed = false;

  /** The attached daemon's `/health`, for capability gating (§2.3 [D-1]). */
  readonly health: Health;

  /**
   * Endpoints that answered `404`, in request order.
   *
   * §2.3 [D-1] says a client hides what the daemon cannot serve. This is the
   * record of *what* was hidden, so "the fleet is empty" and "`/agent/fleet` is
   * not mounted on this build" are distinguishable from outside.
   */
  readonly missing: string[] = [];

  private constructor(
    options: UdsClientOptions,
    health: Health,
    snapshot: Snapshot,
  ) {
    this.socket = options.socket;
    this.clock = options.now ?? (() => Date.now());
    this.communityName = options.communityName ?? "buzz";
    this.relayUrl = options.relayUrl ?? "";
    this.health = health;
    this.snapshot = snapshot;
  }

  /**
   * Perform the §2.3 attach handshake and return a connected client.
   *
   * Throws when the socket is unreachable or the daemon is below the API floor.
   * Both are conditions where continuing would produce a screen that lies, so
   * neither is degraded into a partial attach.
   */
  static async connect(options: UdsClientOptions): Promise<UdsClient> {
    const health = await requestJson<Health>(options.socket, "/health");
    const check = checkApiVersion(health);
    if (!check.ok) throw new Error(check.message);

    // Built with an empty snapshot, then filled by the same `refresh` the
    // stream uses. One code path for boot and for update means a field that is
    // wrong after a live event is wrong at boot too, where a test can see it.
    const client = new UdsClient(options, health, emptySnapshot(options));
    await client.refresh();
    return client;
  }

  getSnapshot(): Snapshot {
    return this.snapshot;
  }

  /**
   * Re-read every mounted collection into one snapshot.
   *
   * Whole-snapshot re-read rather than incremental patching, matching what
   * `Shell.tsx` already does on every stream frame: the daemon is
   * authoritative, and a client-side patch path would be a second state machine
   * to keep in agreement with it. The reads are issued **concurrently** because
   * they are independent and the socket is local — serially they would be
   * seven round trips of latency for no ordering guarantee anyone uses.
   */
  async refresh(): Promise<void> {
    const nowMs = this.clock();
    const [session, channels, fleet] = await Promise.all([
      this.get("/session"),
      this.get("/channel"),
      this.get("/agent/fleet"),
    ]);

    const channelRows: Channel[] = asArray(channels, "channels")
      .map(decodeChannel)
      .filter((c): c is Channel => c !== undefined);
    const agents: Agent[] = asArray(fleet, "agents")
      .map((row) => decodeAgent(row, nowMs))
      .filter((a): a is Agent => a !== undefined);

    // Per-agent activity and usage, one request each and **only for agents the
    // fleet actually reported** — an unbounded fan-out over a directory would
    // be a request storm on a box with many idle agents.
    //
    // Two endpoints rather than one because they are two endpoints: `/activity`
    // returns `{frames}` and nothing else (`api.rs`'s `agent_activity`), and
    // usage lives on `/metric`. An earlier draft read a `usage` field off the
    // activity body, which would have decoded to `undefined` forever and shown
    // every agent as reporting nothing — silently, since §3.4.1's null rule
    // makes "not reported" a legitimate rendering.
    const transcripts: Record<string, TranscriptRow[]> = {};
    const usage: Record<string, Usage> = {};
    await Promise.all(
      agents.map(async (agent) => {
        const [activity, metric] = await Promise.all([
          this.get(`/agent/${agent.pubkey}/activity`),
          this.get(`/agent/${agent.pubkey}/metric`),
        ]);
        const rows = asArray(activity, "frames")
          .map(decodeTranscriptRow)
          .filter((r): r is TranscriptRow => r !== undefined);
        if (rows.length > 0) transcripts[agent.pubkey] = rows;
        const decoded = decodeUsage(asObject(metric)?.metric ?? metric);
        if (decoded) usage[agent.pubkey] = decoded;
      }),
    );

    this.snapshot = {
      session: decodeSession(session, {
        relayUrl: this.relayUrl,
        communityName: this.communityName,
      }),
      // One community per daemon (§2.2). The rail's multi-community fan-out is
      // N clients in the TUI, not N entries from one daemon, so this list is
      // exactly one row and it is derived rather than fetched.
      communities: [
        {
          id: this.socket,
          name: this.communityName,
          relayUrl: this.relayUrl,
          unread: channelRows.reduce((sum, c) => sum + c.unread, 0),
          active: true,
        },
      ],
      channels: channelRows,
      agents,
      // The attention groups, live threads, and huddle rows come from
      // `/mention/inbox` and the forum/huddle endpoints, none of which this
      // build mounts. Empty is the honest value and `missing` records why.
      attention: [],
      threads: [],
      huddles: [],
      messages: {},
      transcripts,
      usage,
      mentionCandidates: [],
      // Copied, not aliased: `missing` keeps growing as later requests 404, and
      // a snapshot that mutated underneath a render would make the same frame
      // disagree with itself.
      missing: [...this.missing],
    };
  }

  /**
   * Load the mention candidates for one channel (§3.3, [D-2]).
   *
   * Per-channel rather than global because that is the endpoint's shape: the
   * roster scoping is what makes a candidate list mean "people who are in this
   * room", and a global list would rank strangers alongside them.
   */
  async loadMentionCandidates(channelId: string): Promise<void> {
    const body = await this.get(
      `/mention/candidates?channel=${encodeURIComponent(channelId)}`,
    );
    const candidates = asArray(body, "candidates")
      .map(decodeMentionCandidate)
      .filter((c): c is NonNullable<typeof c> => c !== undefined);
    this.snapshot = { ...this.snapshot, mentionCandidates: candidates };
  }

  /**
   * Subscribe to `GET /event`.
   *
   * One reader for all listeners: the stream is opened on the first subscribe
   * and closed when the last unsubscribes, so N screens do not open N sockets.
   */
  subscribe(listener: StreamListener): () => void {
    this.listeners.add(listener);
    if (this.listeners.size === 1) void this.readStream();
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) this.stopStream();
    };
  }

  /** Close the stream and refuse further reconnects. */
  close(): void {
    this.closed = true;
    this.stopStream();
    this.listeners.clear();
  }

  private stopStream(): void {
    this.streamAbort?.abort();
    this.streamAbort = null;
    // Retires whichever loop is running, including one asleep on the ladder.
    this.streamGeneration += 1;
  }

  /**
   * Read the ndjson stream until it ends, then reconnect on §2.6's ladder.
   *
   * Two properties the naive loop loses:
   *
   * 1. **A partial line is never parsed.** ndjson frames are split across TCP
   *    reads at arbitrary byte offsets, so the buffer is carried across reads
   *    and only complete lines are decoded. Parsing per-chunk would drop every
   *    frame that happened to straddle a boundary — rare enough to pass a test
   *    and common enough to lose messages in production.
   * 2. **The cursor only advances on data frames.** `stream.reset` carries no
   *    seq; advancing on it would rewind the cursor to 0 and replay the ring.
   */
  private async readStream(): Promise<void> {
    // Claimed at entry and re-checked after every await. A loop whose
    // generation has been retired exits rather than reconnecting — see
    // {@link streamGeneration} for the duplicate-delivery bug that causes.
    this.streamGeneration += 1;
    const generation = this.streamGeneration;
    const retired = (): boolean =>
      this.closed ||
      this.listeners.size === 0 ||
      this.streamGeneration !== generation;

    let attempt = 0;
    while (!retired()) {
      const abort = new AbortController();
      this.streamAbort = abort;
      try {
        const path = this.cursor > 0 ? `/event?since=${this.cursor}` : "/event";
        const response = await fetch(`${BASE}${path}`, {
          unix: this.socket,
          signal: abort.signal,
          headers: { accept: "application/x-ndjson" },
        });
        if (!response.ok || !response.body) {
          // `/event` is unmounted on this build. Recording it and stopping is
          // right: retrying a 404 on a backoff ladder forever would be a busy
          // loop against a route that will not appear without a restart.
          if (response.status === 404) {
            this.missing.push("/event");
            return;
          }
          throw new Error(`GET /event: ${response.status}`);
        }
        attempt = 0;
        await this.consume(response.body, generation);
      } catch (error) {
        // An abort is this client closing the stream on purpose, not a failure.
        if (abort.signal.aborted) return;
        void error;
      }
      if (retired()) return;
      attempt += 1;
      await sleep(backoffMs(attempt));
    }
  }

  /** Decode complete ndjson lines out of a byte stream. */
  private async consume(
    body: ReadableStream<Uint8Array>,
    generation: number,
  ): Promise<void> {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      // A retired loop stops delivering immediately rather than draining what
      // is already buffered: after an unsubscribe those frames belong to a
      // listener set that no longer wants them, and after a `close()` they
      // would arrive at a client the caller believes is shut down.
      if (this.streamGeneration !== generation) {
        await reader.cancel().catch(() => {});
        return;
      }
      buffer += decoder.decode(value, { stream: true });
      let newline = buffer.indexOf("\n");
      while (newline >= 0) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        if (line.length > 0) this.dispatch(line);
        newline = buffer.indexOf("\n");
      }
    }
  }

  /** Apply one decoded line: update the snapshot, advance the cursor, fan out. */
  private dispatch(line: string): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(line);
    } catch {
      // A malformed line is the daemon's bug, and this process owns the
      // terminal so it cannot be logged to stderr without corrupting the
      // frame. Dropping one line is strictly better than tearing down a live
      // stream over it, and the cursor does not advance, so a reconnect
      // replays it.
      return;
    }
    const frame = decodeStreamFrame(parsed);
    if (!frame) return;

    if (frame.type === "stream.reset") {
      // §2.6: the cursor aged out of the ring. Invalidate and re-fetch rather
      // than presenting a silently gapped timeline — and reset the cursor
      // first, or the re-subscribe would ask for the same dead seq again.
      this.cursor = 0;
      // The `catch` is load-bearing, not defensive noise. `refresh` throws on
      // any non-404, and this runs fire-and-forget from a stream reader — an
      // unhandled rejection under Bun's default can take the process down,
      // turning "the daemon restarted while I was away" into a lost session.
      // The frame is emitted either way: the screens must learn the stream
      // reset even if the re-fetch that follows it failed, because the
      // alternative is a client that silently keeps rendering pre-gap state.
      void this.refresh()
        .catch(() => {})
        .finally(() => this.emit(frame));
      return;
    }

    if (advancesCursor(frame)) this.cursor = frame.seq;
    this.applyFrame(frame);
    this.emit(frame);
  }

  /**
   * Fold a data frame into the local snapshot.
   *
   * Only `message.new` is folded in place. Everything else triggers a whole
   * re-read on the next paint through `Shell`'s subscription — the daemon is
   * authoritative and a per-topic patch path is the second state machine this
   * design keeps refusing. The message case earns its exception because it is
   * the one frame whose latency the operator can feel.
   */
  private applyFrame(frame: StreamFrame): void {
    if (frame.type !== "message.new") return;
    const message = decodeMessage(frame.data);
    if (!message) return;
    const existing = this.snapshot.messages[message.channelId] ?? [];
    // Idempotent by event id: a reconnect replays from the ring, so the same
    // message can arrive twice. Appending blind would double every message
    // across a reconnect — the exact bug `?since=` exists to avoid.
    if (existing.some((m) => m.id === message.id)) return;
    this.snapshot = {
      ...this.snapshot,
      messages: {
        ...this.snapshot.messages,
        [message.channelId]: [...existing, message],
      },
    };
  }

  private emit(frame: StreamFrame): void {
    for (const listener of this.listeners) listener(frame);
  }

  async send(
    channelId: string,
    content: string,
    options: { replyTo?: string; mentions?: readonly string[] } = {},
  ): Promise<Message> {
    const body = await this.post(
      `/channel/${encodeURIComponent(channelId)}/message`,
      {
        content,
        ...(options.replyTo ? { reply_to: options.replyTo } : {}),
        mentions: options.mentions ?? [],
      },
    );
    const eventId = asString(body, "event_id") ?? "";
    // The daemon returns `{event_id, accepted, message}` rather than a
    // hydrated message: the hydrated one arrives on the stream as
    // `message.new`, which is the same path a message from anyone else takes.
    // Synthesizing the echo here keeps `DaemonClient.send`'s contract without
    // pretending to know a `created_at` the relay assigned.
    return {
      id: eventId,
      channelId,
      author: {
        pubkey: this.snapshot.session.pubkey,
        name: this.snapshot.session.name,
        isAgent: false,
      },
      ts: this.clock(),
      content,
      ...(options.replyTo ? { replyTo: options.replyTo } : {}),
      ...(options.mentions ? { mentions: [...options.mentions] } : {}),
    };
  }

  async markRead(channelId: string, eventId?: string): Promise<void> {
    await this.post(`/channel/${encodeURIComponent(channelId)}/read`, {
      ...(eventId ? { event_id: eventId } : {}),
    });
  }

  /**
   * `GET` a path, returning `undefined` on `404` and recording the absence.
   *
   * Every other non-2xx throws: a `500` from the daemon is a fault the operator
   * needs to see, and treating it like an unmounted route would hide a broken
   * daemon behind an empty screen.
   */
  private async get(path: string): Promise<unknown> {
    try {
      return await requestJson<unknown>(this.socket, path);
    } catch (error) {
      if (error instanceof DaemonRequestError && error.status === 404) {
        this.missing.push(path);
        return undefined;
      }
      throw error;
    }
  }

  private async post(path: string, body: unknown): Promise<unknown> {
    return requestJson<unknown>(this.socket, path, {
      method: "POST",
      body: JSON.stringify(body),
      headers: { "content-type": "application/json" },
    });
  }
}

/** One HTTP round trip over the socket, with the §2.4 error model applied. */
async function requestJson<T>(
  socket: string,
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const response = await fetch(`${BASE}${path}`, { ...init, unix: socket });
  const text = await response.text();
  let parsed: unknown;
  try {
    parsed = text.length > 0 ? JSON.parse(text) : {};
  } catch {
    parsed = {};
  }
  if (!response.ok) {
    const error = asObject(asObject(parsed)?.error);
    throw new DaemonRequestError(response.status, {
      code: asString(error, "code") ?? "unknown",
      // The status is in the message because the daemon's own body may be
      // absent (a 404 from the router never reaches a handler), and "not found"
      // with no path is a dead end §1.3 property 2 forbids.
      message: asString(error, "message") ?? `${response.status} on ${path}`,
    });
  }
  return parsed as T;
}

/** §2.6: link A "reconnects on the same exponential shape (1 s → 30 s cap)". */
const MAX_BACKOFF_MS = 30 * MS_PER_SECOND;

/** The §2.6 link-A ladder: exponential from 1 s, capped at 30 s. */
export function backoffMs(attempt: number): number {
  const base = MS_PER_SECOND * 2 ** Math.max(0, attempt - 1);
  return Math.min(base, MAX_BACKOFF_MS);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function asObject(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function asArray(value: unknown, key: string): unknown[] {
  const field = asObject(value)?.[key];
  return Array.isArray(field) ? field : [];
}

function asString(
  value: Record<string, unknown> | unknown,
  key: string,
): string | undefined {
  const field = asObject(value)?.[key];
  return typeof field === "string" ? field : undefined;
}

/** The snapshot a client holds before its first `refresh`. */
function emptySnapshot(options: UdsClientOptions): Snapshot {
  return {
    session: {
      pubkey: "",
      name: "",
      relayUrl: options.relayUrl ?? "",
      communityName: options.communityName ?? "buzz",
      connection: { state: "connecting" },
      // Keyless until the daemon says otherwise (§2.5): the pre-handshake
      // snapshot must not render as a healthy archiving daemon.
      archiving: false,
    },
    communities: [],
    channels: [],
    agents: [],
    attention: [],
    threads: [],
    huddles: [],
    messages: {},
    transcripts: {},
    usage: {},
    mentionCandidates: [],
  };
}
