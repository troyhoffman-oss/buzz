/**
 * The fixture transport — DESIGN.md §5.4.
 *
 * > ```
 * > BUZZ_TUI_FIXTURE=/path/scenario.jsonl   → in-process fake transport
 * > BUZZ_DAEMON_SOCKET=/path/x.sock         → real daemon
 * > ```
 * > A scenario is ordered JSONL of `{atMs, kind, payload}` replayed on the
 * > frozen clock: an initial state snapshot, then the event stream.
 *
 * **This lives in `src/`, not `test/`, because `BUZZ_TUI_FIXTURE` is a product
 * environment variable.** §5.5's tmux runs and §5.6's recordings both launch
 * the *shipped* binary against a fixture; a transport that only existed under
 * `test/` would be absent from the compiled artifact and every T2 sequence in
 * §5.5 would be undrivable. It is also what makes `just tui-dev` useful before
 * the daemon lands.
 *
 * Two properties this implementation is built around, both of which the obvious
 * version loses:
 *
 * 1. **Replay is driven by an explicit clock, not `setTimeout`.** T1 snapshots
 *    must be deterministic (§5.3 requirement 1) and T2 runs must be able to
 *    advance time faster than wall-clock. So `advanceTo(ms)` delivers every
 *    frame at or before that offset, and nothing delivers on its own. Real
 *    timers would make "two runs of one input produce byte-identical buffers"
 *    (§5.3's shadow run) a coin flip.
 *
 * 2. **It is the same shape as the real client.** {@link FixtureClient}
 *    implements {@link DaemonClient} — the interface the UDS client will also
 *    implement — so swapping `BUZZ_TUI_FIXTURE` for `BUZZ_DAEMON_SOCKET` is a
 *    constructor change and nothing else. A richer test-only surface would mean
 *    every test exercised a shape the product does not have.
 *
 * The daemon lane owns wire truth. When `crates/buzz-daemon` publishes its
 * OpenAPI document, the generated client replaces `src/client/daemon-client.ts`
 * and this file is checked against the same document — the scenarios under
 * `fixtures/` are the contract that survives that swap.
 */

import { readFileSync } from "node:fs";
import type { DaemonClient, StreamListener } from "./daemon-client";
import type { Message, Snapshot, TranscriptRow, Usage } from "./types";

/** One line of a scenario file (§5.4). */
export interface FixtureLine {
  readonly atMs: number;
  readonly kind: string;
  readonly payload: unknown;
}

/** Parse a scenario file. Rejects an empty or snapshot-less scenario loudly. */
export function parseFixture(text: string): FixtureLine[] {
  const lines = text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0)
    .map((l, i) => {
      try {
        return JSON.parse(l) as FixtureLine;
      } catch (cause) {
        throw new Error(`fixture line ${i + 1} is not JSON`, { cause });
      }
    });
  if (lines.length === 0) throw new Error("fixture is empty");
  const first = lines[0];
  if (!first || first.kind !== "snapshot") {
    // §5.4: "an initial state snapshot, then the event stream". A scenario
    // without one would boot the TUI into an undefined state and the resulting
    // snapshot diff would look like a renderer bug.
    throw new Error("fixture must open with a 'snapshot' line");
  }
  return lines;
}

/**
 * An in-process daemon backed by a fixture file.
 *
 * Mutations are applied to the local snapshot and echoed on the stream, exactly
 * as `POST` + `message.new` does on the wire (`daemon-api.md` §3.3's
 * optimistic-echo contract). That is what lets a T2 run type into the composer,
 * press `⏎`, and assert the message appears — without it, send would be a
 * no-op that every test would have to work around.
 */
export class FixtureClient implements DaemonClient {
  private readonly lines: FixtureLine[];
  private cursor = 0;
  private now = 0;
  private seq = 0;
  private listeners = new Set<StreamListener>();
  private snapshot: Snapshot;
  /** Messages keyed by channel; mutable so sends land. */
  private messages: Map<string, Message[]>;
  private transcripts: Map<string, TranscriptRow[]>;
  private usage: Map<string, Usage>;
  private sentCounter = 0;
  /**
   * Wall-clock time the scenario's `atMs: 0` corresponds to.
   *
   * **Derived from the scenario, not hardcoded.** A literal epoch here would
   * mean every fixture had to be authored against this file's opinion of "now",
   * and a scenario written a month later would render its newest message as a
   * month-old one. Taking the newest event in the snapshot makes `atMs: 0` mean
   * "the moment the scenario was captured", which is what an author writing
   * `T0 - 3 * MIN` already assumes.
   */
  private readonly epoch: number;

  constructor(fixtureText: string) {
    this.lines = parseFixture(fixtureText);
    const first = this.lines[0];
    if (!first)
      throw new Error("unreachable: parseFixture guarantees a snapshot");
    this.snapshot = first.payload as Snapshot;
    this.cursor = 1;
    this.messages = new Map(
      Object.entries(this.snapshot.messages).map(([k, v]) => [k, [...v]]),
    );
    this.transcripts = new Map(
      Object.entries(this.snapshot.transcripts).map(([k, v]) => [k, [...v]]),
    );
    this.usage = new Map(Object.entries(this.snapshot.usage));
    this.epoch = Math.max(
      0,
      ...[...this.messages.values()].flat().map((m) => m.ts),
      ...[...this.transcripts.values()].flat().map((r) => r.ts),
    );
  }

  /** Load a scenario by name from `fixtures/`. */
  static fromFile(name: string): FixtureClient {
    const path = new URL(`../../fixtures/${name}.jsonl`, import.meta.url)
      .pathname;
    return new FixtureClient(readFileSync(path, "utf8"));
  }

  getSnapshot(): Snapshot {
    return {
      ...this.snapshot,
      messages: Object.fromEntries(this.messages),
      transcripts: Object.fromEntries(this.transcripts),
      usage: Object.fromEntries(this.usage),
    };
  }

  subscribe(listener: StreamListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /**
   * Advance the frozen clock to `ms` and deliver every frame due at or before it.
   *
   * Frames are delivered in file order, which for a well-formed scenario is
   * timestamp order. Sorting here instead would silently repair a scenario
   * whose events are out of order — and an out-of-order scenario is a fixture
   * bug worth failing on rather than smoothing over.
   */
  advanceTo(ms: number): void {
    this.now = ms;
    while (this.cursor < this.lines.length) {
      const line = this.lines[this.cursor];
      if (!line || line.atMs > ms) break;
      this.cursor++;
      this.apply(line);
    }
  }

  /** The current frozen-clock offset, in ms from the scenario's `atMs` 0. */
  currentTime(): number {
    return this.now;
  }

  /** True when every scenario line has been delivered. */
  isDrained(): boolean {
    return this.cursor >= this.lines.length;
  }

  private apply(line: FixtureLine): void {
    switch (line.kind) {
      case "message.new": {
        const message = line.payload as Message;
        this.appendMessage(message);
        this.emit("message.new", message.channelId, message);
        break;
      }
      case "agent.frame": {
        const { agentPubkey, row } = line.payload as {
          agentPubkey: string;
          row: TranscriptRow;
        };
        const rows = this.transcripts.get(agentPubkey) ?? [];
        this.transcripts.set(agentPubkey, [...rows, row]);
        this.emit("agent.frame", undefined, line.payload);
        break;
      }
      case "agent.metric": {
        const { agentPubkey, usage } = line.payload as {
          agentPubkey: string;
          usage: Usage;
        };
        this.usage.set(agentPubkey, usage);
        this.emit("agent.metric", undefined, line.payload);
        break;
      }
      case "connection.state": {
        this.snapshot = {
          ...this.snapshot,
          session: {
            ...this.snapshot.session,
            connection: line.payload as Snapshot["session"]["connection"],
          },
        };
        this.emit("connection.state", undefined, line.payload);
        break;
      }
      default:
        // §3.4.1's rule, applied to the transport: "anything unrecognized is
        // dropped, never guessed". An unknown frame kind must not become a
        // half-applied state change.
        this.emit(line.kind, undefined, line.payload);
    }
  }

  private appendMessage(message: Message): void {
    const existing = this.messages.get(message.channelId) ?? [];
    this.messages.set(message.channelId, [...existing, message]);
  }

  private emit(
    type: string,
    channelId: string | undefined,
    data: unknown,
  ): void {
    this.seq += 1;
    const frame = { seq: this.seq, type, channel_id: channelId, data };
    for (const listener of this.listeners) listener(frame);
  }

  async send(
    channelId: string,
    content: string,
    options: { replyTo?: string; mentions?: readonly string[] } = {},
  ): Promise<Message> {
    this.sentCounter += 1;
    const message: Message = {
      id: `ev_sent_${String(this.sentCounter).padStart(3, "0")}`,
      channelId,
      author: {
        pubkey: this.snapshot.session.pubkey,
        name: this.snapshot.session.name,
        isAgent: false,
      },
      ts: this.epoch + this.now,
      content,
      ...(options.replyTo ? { replyTo: options.replyTo } : {}),
      ...(options.mentions ? { mentions: [...options.mentions] } : {}),
    };
    this.appendMessage(message);
    this.emit("message.new", channelId, message);
    return message;
  }

  async markRead(channelId: string, eventId?: string): Promise<void> {
    const markers = { ...(this.snapshot.readMarkers ?? {}) };
    const messages = this.messages.get(channelId) ?? [];
    const last = messages.at(-1);
    const anchor = eventId ?? last?.id;
    if (anchor) markers[channelId] = anchor;
    this.snapshot = {
      ...this.snapshot,
      readMarkers: markers,
      channels: this.snapshot.channels.map((c) =>
        c.id === channelId ? { ...c, unread: 0, mentions: 0 } : c,
      ),
    };
    this.emit("read_state.update", channelId, { channelId, eventId: anchor });
  }

  /**
   * No-op: a scenario carries its own messages, so there is nothing to fetch.
   *
   * The parameter is deliberately unused. Faking a fetch here — even a delay —
   * would make the fixture transport model the socket transport's *latency*
   * without modelling its *failures*, which is the worst of both: tests would
   * become timing-dependent while still never exercising a real error path.
   */
  async ensureMessages(_channelId: string): Promise<void> {}
}
