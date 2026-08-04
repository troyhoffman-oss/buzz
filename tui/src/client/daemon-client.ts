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
}
