/**
 * The one interface between the TUI and everything behind it — DESIGN.md §2.1,
 * §6.4.
 *
 * > The **front end is deliberately disposable**: its whole network layer is
 * > one HTTP client and one line reader. It never parses an event, sees a key,
 * > knows a relay URL, or learns a kind number.
 *
 * Both the fixture transport (`test/mock-daemon.ts`, §5.4) and the real UDS
 * client implement this. Keeping it this small is the point: every method here
 * is a shape the daemon already produces, so a `ratatui` front end written
 * against the same daemon loses zero protocol work.
 *
 * TODO(wave1, §4.1.1 deliverable 14): replaced by the **generated** client once
 * `just daemon-spec-check` regenerates the OpenAPI document. The interface is
 * declared here rather than imported from the generated module so the screens
 * do not need to change when that lands.
 */

import type { Message, Snapshot, StreamFrame } from "./types";

/** A subscriber to `GET /event` (`daemon-api.md` §4.1). */
export type StreamListener = (frame: StreamFrame) => void;

/** Everything the Wave-1 screens need from the daemon. */
export interface DaemonClient {
  /** The current hydrated state — the boot `GET`s, folded into one object. */
  getSnapshot(): Snapshot;

  /**
   * Subscribe to the one event stream. Returns an unsubscribe function.
   *
   * There is no second socket, no per-channel connection, and no long-poll
   * fallback (`daemon-api.md` §4.1) — that is the property that keeps this
   * interface at six methods.
   */
  subscribe(listener: StreamListener): () => void;

  /**
   * Send a message. `mentions` carries **resolved pubkeys**, never names
   * ([D-2]): what the composer picked is what gets tagged, by construction.
   */
  send(
    channelId: string,
    content: string,
    options?: { replyTo?: string; mentions?: readonly string[] },
  ): Promise<Message>;

  /** Mark a channel read to an event id, or to now (`POST /channel/{id}/read`). */
  markRead(channelId: string, eventId?: string): Promise<void>;

  /**
   * Ensure a channel's timeline is loaded, if this transport has to fetch it.
   *
   * On the socket transport this is `GET /channel/{id}/message` and it is the
   * only way a timeline is ever populated — the boot snapshot deliberately
   * carries none, because a machine with forty channels would otherwise make
   * forty relay round trips before the first frame paints. It also has a
   * **side effect the client depends on**: the daemon registers a live
   * subscription for the channel when it serves a page, anchored to the newest
   * row in it, so fetching history is also how the tail goes live.
   *
   * On the fixture transport it is a no-op: the scenario carries its own
   * messages and there is nothing to fetch. That asymmetry is why this is a
   * method on the interface rather than a call in the shell against a concrete
   * client — the shell should not know which transport it holds.
   *
   * Idempotent by contract, because descent is a common keystroke and `←` `→`
   * must not re-query the relay each time.
   */
  ensureMessages(channelId: string): Promise<void>;

  /**
   * A counter that changes whenever loaded timelines are invalidated.
   *
   * Exists because `ensureMessages` is idempotent and the consumer that drives
   * it is keyed on the *current channel*: after a `stream.reset` the pages are
   * stale but the channel has not changed, so nothing would re-run and the
   * timeline would render blank permanently. Depending on this alongside the
   * channel id makes "the same channel, but re-fetch it" expressible.
   *
   * Constant on a transport that never invalidates (the fixture one).
   */
  messagesGeneration(): number;
}
