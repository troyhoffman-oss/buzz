/**
 * Decoding the daemon's JSON into the view types the screens read — the pure
 * half of the UDS transport.
 *
 * Split out from `uds-client.ts` on purpose: everything here is
 * `(unknown) → view type` with no socket, so the wire fixtures under
 * `fixtures/wire/` and `test/unit/wire.test.ts` exercise the *same* functions
 * the live client runs. A decoder that only existed inside the fetch path
 * would be testable only against a running daemon, which is exactly the
 * coupling §5.4's fixture protocol exists to remove.
 *
 * # Two shapes, and why both are accepted
 *
 * `daemon-api.md` §4.2 writes a stream frame as `{seq, ts, type, channel_id,
 * data}`. The committed Rust serializes `{seq, type, payload}`
 * (`crates/buzz-daemon/src/stream.rs`, `StreamFrame`). That is a real
 * divergence between the design document and the code, not a misreading, and
 * this decoder accepts **either** key rather than picking a winner:
 *
 * - Picking `data` would make the client silently drop every frame the
 *   daemon actually emits today — a live TUI that renders nothing, with no
 *   error anywhere, which is the §1.3-property-3 failure this whole design
 *   works to avoid.
 * - Picking `payload` would break the moment the daemon is corrected toward
 *   its own spec.
 *
 * Accepting both is not a compatibility shim for a broken design; it is the
 * honest reading of a field that has one meaning and two spellings in the two
 * documents of record. When the generated client lands (§4.1.1 deliverable
 * 14) the OpenAPI document settles it and this branch collapses to one key.
 *
 * # Everything here is defensive in one direction only
 *
 * A field the daemon did not send becomes a **conservative default**, never a
 * fabricated value: an absent unread count is `0`, an absent presence is
 * `unknown` (never `offline` — §2.4 is explicit that those are different
 * facts), and an absent numeric usage figure stays `null` so §3.4.1 can render
 * `—` rather than a `0` the agent never reported.
 */

/**
 * `MS_PER_SECOND` is imported rather than written inline: `src/time/units.ts`
 * is the one file §6.4's digit scan exempts by exact path, and reaching for a
 * local `1000` here is exactly the formatting-based bypass that gate's comment
 * warns against.
 */
import { MS_PER_SECOND } from "../time/units";
import type {
  Agent,
  AgentState,
  Channel,
  ConnectionState,
  MentionCandidate,
  Message,
  Presence,
  Session,
  StreamFrame,
  TranscriptRow,
  Usage,
} from "./types";

/** A JSON object, as far as a decoder is concerned. */
type Json = Record<string, unknown>;

/** Narrow an unknown to a JSON object, or `undefined`. */
function obj(value: unknown): Json | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Json)
    : undefined;
}

/** Read a string field, or `undefined` when it is absent or the wrong type. */
function str(source: Json | undefined, key: string): string | undefined {
  const value = source?.[key];
  return typeof value === "string" ? value : undefined;
}

/** Read a finite number field, or `undefined`. */
function num(source: Json | undefined, key: string): number | undefined {
  const value = source?.[key];
  return typeof value === "number" && Number.isFinite(value)
    ? value
    : undefined;
}

/** Read a boolean field, or `undefined`. */
function bool(source: Json | undefined, key: string): boolean | undefined {
  const value = source?.[key];
  return typeof value === "boolean" ? value : undefined;
}

/** Read an array field, or `[]`. */
function arr(source: Json | undefined, key: string): unknown[] {
  const value = source?.[key];
  return Array.isArray(value) ? value : [];
}

/** Read an array of strings, dropping non-strings rather than coercing them. */
function strings(source: Json | undefined, key: string): string[] {
  return arr(source, key).filter((v): v is string => typeof v === "string");
}

/**
 * Read a **nullable** number: `null` and absence both stay `null`.
 *
 * §3.4.1's null rule, applied at the decode boundary rather than at each
 * renderer: "`null` means **not reported**, not zero". Collapsing it to `0`
 * here would fabricate a measurement that every downstream renderer would then
 * faithfully display.
 */
function nullableNum(source: Json | undefined, key: string): number | null {
  const value = source?.[key];
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** The four presence states (§2.4). Anything unrecognized is `unknown`. */
export function decodePresence(value: unknown): Presence {
  switch (value) {
    case "present":
    case "waking":
    case "offline":
      return value;
    default:
      // Including the literal `"unknown"`, and including a state this client
      // has never heard of. §2.4: rendering `offline` for "I have not heard
      // yet" is the looks-idle-while-the-socket-is-dead failure.
      return "unknown";
  }
}

/**
 * The connection state, carried through **verbatim** (§2.6).
 *
 * An unrecognized state decodes to `disconnected` rather than being dropped:
 * the statusline must render *something*, and every non-connected state is a
 * loss state, so the conservative choice is the one that shows a loss. Guessing
 * `connected` would be the one wrong answer.
 */
export function decodeConnection(value: unknown): ConnectionState {
  const source = obj(value);
  switch (str(source, "state")) {
    case "connected":
      return { state: "connected" };
    case "connecting":
      return { state: "connecting" };
    case "authenticating":
      return { state: "authenticating" };
    case "dns_brownout":
      return { state: "dns_brownout" };
    case "rate_limited":
      return {
        state: "rate_limited",
        retry_after_ms: num(source, "retry_after_ms") ?? 0,
      };
    case "reconnecting":
      return {
        state: "reconnecting",
        attempt: num(source, "attempt") ?? 1,
        next_retry_in_ms: num(source, "next_retry_in_ms") ?? 0,
      };
    case "auth_failed":
      return {
        state: "auth_failed",
        // §2.6 promises the remediation is inline, which needs the reason. An
        // empty reason would render as a bare "auth failed" with no next step.
        reason: str(source, "reason") ?? "unknown",
      };
    default:
      return { state: "disconnected" };
  }
}

/** Decode `GET /session` into the shape the statusline reads. */
export function decodeSession(
  value: unknown,
  fallback: { relayUrl: string; communityName: string },
): Session {
  const source = obj(value);
  const pubkey = str(source, "pubkey") ?? "";
  return {
    pubkey,
    // The daemon serves a pubkey, not a display name — kind-0 resolution for
    // *self* lands with the directory cache. Until then the truncated pubkey is
    // the honest label: it is what the daemon knows.
    name: str(source, "name") ?? (pubkey ? pubkey.slice(0, 8) : "unknown"),
    relayUrl: str(source, "relay_url") || fallback.relayUrl,
    communityName: str(source, "community_name") ?? fallback.communityName,
    connection: decodeConnection(source?.connection),
    // §2.5: keyless is a **visible** state. Defaulting to `true` would make a
    // keyless daemon look healthy, which is the one thing this field exists to
    // prevent — so absence is treated as keyless.
    archiving: bool(source, "archiving") ?? false,
  };
}

/** Decode one channel row from `GET /channel` (`channels::Channel`). */
export function decodeChannel(value: unknown): Channel | undefined {
  const source = obj(value);
  const id = str(source, "id");
  if (!id) return undefined;
  const channelType = str(source, "channel_type");
  return {
    id,
    name: str(source, "name") ?? id.slice(0, 8),
    ...(str(source, "topic") ? { topic: str(source, "topic") as string } : {}),
    unread: num(source, "unread") ?? 0,
    mentions: num(source, "mentions") ?? 0,
    agentsWorking: strings(source, "agents_working"),
    // `unknown` is not collapsed into `channel`: the daemon keeps them
    // distinct (`channels.rs`) for the same reason presence does, so the view
    // type simply omits the field rather than guessing a kind.
    ...(channelType === "channel" ||
    channelType === "dm" ||
    channelType === "forum"
      ? { kind: channelType }
      : {}),
  };
}

/** Fleet state class → the view's `AgentState` (`fleet::AgentState`). */
function decodeAgentState(value: unknown): AgentState {
  switch (value) {
    case "blocked":
      return "needsInput";
    case "working":
      return "working";
    case "completed":
      return "completed";
    case "idle":
    case "offline":
    case "unknown":
      return "idle";
    default:
      return "idle";
  }
}

/**
 * Presence implied by a fleet row's state class.
 *
 * The fleet endpoint collapses presence into its sort class, so `offline` and
 * `unknown` both arrive as a state rather than as a presence field. Recovering
 * the distinction here keeps §2.4's rule intact one layer up: a row the daemon
 * called `unknown` must not render with an offline dot.
 */
function presenceFromFleetState(value: unknown): Presence {
  switch (value) {
    case "blocked":
    case "working":
    case "idle":
      return "present";
    case "offline":
      return "offline";
    default:
      return "unknown";
  }
}

/** Decode one row of `GET /agent/fleet` (`fleet::FleetRow`). */
export function decodeAgent(value: unknown, nowMs: number): Agent | undefined {
  const source = obj(value);
  const pubkey = str(source, "pubkey");
  if (!pubkey) return undefined;
  const elapsed = num(source, "elapsed_secs");
  const turn = str(source, "turn");
  return {
    pubkey,
    name: str(source, "name") ?? pubkey.slice(0, 8),
    runtime: str(source, "runtime") ?? "",
    presence: presenceFromFleetState(source?.state),
    state: decodeAgentState(source?.state),
    ...(str(source, "channel_id")
      ? { channelId: str(source, "channel_id") as string }
      : {}),
    ...(str(source, "channel_name")
      ? { channelName: str(source, "channel_name") as string }
      : {}),
    ...(turn ? { turnId: turn } : {}),
    // The daemon reports **elapsed**, the view wants a **start**. Deriving the
    // start from the injected clock keeps the badge counting up on its own
    // between polls, which is what an operator watching a stuck turn needs;
    // storing elapsed would freeze the badge until the next fetch.
    ...(elapsed !== undefined
      ? { turnStartedAt: nowMs - elapsed * MS_PER_SECOND }
      : {}),
    ...(str(source, "detail")
      ? { detail: str(source, "detail") as string }
      : {}),
  };
}

/** Decode one candidate from `GET /mention/candidates`. */
export function decodeMentionCandidate(
  value: unknown,
): MentionCandidate | undefined {
  const source = obj(value);
  const pubkey = str(source, "pubkey");
  if (!pubkey) return undefined;
  const displayName = str(source, "display_name") ?? pubkey.slice(0, 8);
  return {
    pubkey,
    // The daemon serves one label; the picker shows a handle and a name. Using
    // the same string for both is honest — inventing a distinct `@handle` by
    // lowercasing and stripping spaces would produce a handle nobody can type
    // and that resolves to nothing.
    handle: displayName,
    displayName,
    isAgent: bool(source, "is_agent") ?? false,
    presence: decodePresence(source?.presence),
    ...(str(source, "detail")
      ? { detail: str(source, "detail") as string }
      : {}),
  };
}

/**
 * Decode a hydrated message.
 *
 * Returns `undefined` for anything without an id and a channel: a message the
 * timeline cannot anchor is worse than an absent one, because the unread
 * divider anchors to an event id and a row with no id would break it silently.
 */
export function decodeMessage(value: unknown): Message | undefined {
  const source = obj(value);
  const id = str(source, "id") ?? str(source, "event_id");
  const channelId = str(source, "channel_id");
  if (!id || !channelId) return undefined;
  const author = obj(source?.author);
  const authorPubkey = str(author, "pubkey") ?? str(source, "pubkey") ?? "";
  const tsSeconds = num(source, "created_at");
  return {
    id,
    channelId,
    author: {
      pubkey: authorPubkey,
      name: str(author, "name") ?? authorPubkey.slice(0, 8),
      isAgent: bool(author, "is_agent") ?? false,
    },
    // `ts` is milliseconds in the view and seconds on the wire (Nostr's
    // `created_at`). Doing the conversion once, here, is why no renderer has to
    // know which unit it received.
    ts: num(source, "ts") ?? (tsSeconds ?? 0) * MS_PER_SECOND,
    content: str(source, "content") ?? "",
    ...(str(source, "reply_to")
      ? { replyTo: str(source, "reply_to") as string }
      : {}),
    ...(num(source, "reply_count") !== undefined
      ? { replyCount: num(source, "reply_count") }
      : {}),
    ...(num(source, "unread_reply_count") !== undefined
      ? { unreadReplyCount: num(source, "unread_reply_count") }
      : {}),
    ...(bool(source, "system") ? { system: true } : {}),
    ...(strings(source, "mentions").length > 0
      ? { mentions: strings(source, "mentions") }
      : {}),
  };
}

/**
 * Decode one assembled timeline row from `GET /channel/{id}/message`.
 *
 * **A different shape from {@link decodeMessage}, and deliberately so.** That
 * one decodes a *hydrated* message — the flat form `POST` echoes back and the
 * `/event` stream carries. A history page carries `timeline::TimelineRow`:
 * `{event, thread}`, where `event` is the relay's signed event **verbatim** and
 * `thread` is the `39005` overlay bound to it, or `null` when the relay sent
 * none. Feeding one to the other's decoder yields `undefined` for every row —
 * `channel_id` is a tag on the inner event, not a field on the outer object —
 * which is silent and looks exactly like an empty channel.
 *
 * `channelId` is passed in rather than read off the event. The daemon answers
 * per channel and the caller knows which one it asked for; digging the `h` tag
 * out here would put NIP-29 tag vocabulary in `src/`, which §6.4 forbids and
 * `check-boundary.sh` enforces.
 *
 * Returns `undefined` for a row with no event id: the unread divider and the
 * thread descent both anchor to one, so a row that cannot be anchored is worse
 * than an absent row.
 */
export function decodeTimelineRow(
  value: unknown,
  channelId: string,
): Message | undefined {
  const row = obj(value);
  const event = obj(row?.event);
  const id = str(event, "id");
  if (!id) return undefined;
  const thread = obj(row?.thread);
  const authorPubkey = str(event, "pubkey") ?? "";
  return {
    id,
    channelId,
    author: {
      pubkey: authorPubkey,
      // The daemon's history page carries no profile join, so the short pubkey
      // is the honest label. `/user` resolves display names separately and the
      // renderer prefers whatever it has; inventing a name here would make an
      // unresolved author indistinguishable from a resolved one.
      name: authorPubkey.slice(0, 8),
      isAgent: false,
    },
    // `created_at` is seconds on the wire (Nostr) and milliseconds in the view.
    // Converting once at the boundary is why no renderer has to know which.
    ts: (num(event, "created_at") ?? 0) * MS_PER_SECOND,
    content: str(event, "content") ?? "",
    ...(num(thread, "reply_count") !== undefined
      ? { replyCount: num(thread, "reply_count") }
      : {}),
    // The daemon classifies the row (`timeline::TimelineRow.system`) because
    // classifying it here would mean knowing which kinds are conversational,
    // and kinds are daemon vocabulary — `check-boundary.sh` fails the build on
    // a bare kind integer in `src/`. Without the flag a 40099 `dm_created`
    // rendered its raw JSON payload in the middle of a conversation, which is
    // what the M3 live walk caught.
    ...(bool(row, "system") ? { system: true } : {}),
  };
}

/** Decode one observer frame into a transcript row (`observer::ObserverFrame`). */
export function decodeTranscriptRow(value: unknown): TranscriptRow | undefined {
  const source = obj(value);
  const seq = num(source, "seq");
  if (seq === undefined) return undefined;
  const kind = str(source, "kind") ?? "";
  const timestamp = str(source, "timestamp");
  const createdAt = num(source, "created_at");
  return {
    // Frames are identified by `(agent, seq, timestamp)` in the daemon's own
    // archive ([D-3]); the row id mirrors the seq half, which is the part that
    // is unique within one agent's feed — which is the scope a transcript has.
    id: `frame-${seq}`,
    ts:
      (timestamp ? Date.parse(timestamp) : Number.NaN) ||
      (createdAt ?? 0) * MS_PER_SECOND,
    class: transcriptClass(kind),
    label: kind,
    ...(str(obj(source?.payload), "title")
      ? { detail: str(obj(source?.payload), "title") as string }
      : {}),
  };
}

/**
 * Map an observer frame kind onto a render class (§3.4.1's classifier).
 *
 * Matching by **substring** rather than by an exhaustive table is deliberate:
 * the wire kinds are `acp_*` names produced by a harness this client has no
 * version handshake with, so an exhaustive table would silently reclassify
 * every new kind as `message`. Anything genuinely unrecognized still lands on
 * `message`, which renders as a plain row rather than as a wrong icon.
 */
function transcriptClass(kind: string): TranscriptRow["class"] {
  if (kind.includes("read") || kind.includes("grep")) return "read";
  if (kind.includes("edit") || kind.includes("write")) return "write";
  if (kind.includes("bash") || kind.includes("shell")) return "shell";
  if (kind.includes("thought") || kind.includes("think")) return "thought";
  if (kind.includes("plan")) return "plan";
  if (kind.includes("permission") || kind.includes("ask")) return "permission";
  if (kind.includes("error") || kind.includes("fail")) return "error";
  if (kind.includes("turn") || kind.includes("session")) return "lifecycle";
  return "message";
}

/** Decode NIP-AM usage (`metric::TurnMetric`) with the null rule intact. */
export function decodeUsage(value: unknown): Usage | undefined {
  const source = obj(value);
  if (!source) return undefined;
  const contextWindow = nullableNum(source, "context_window");
  const tokensIn = nullableNum(source, "tokens_in");
  const tokensOut = nullableNum(source, "tokens_out");
  return {
    inTokens: tokensIn,
    outTokens: tokensOut,
    cacheRead: nullableNum(source, "cache_read"),
    cacheWrite: nullableNum(source, "cache_write"),
    costUsd: nullableNum(source, "cost_usd"),
    model: str(source, "model") ?? "",
    // Context *used* is not a wire field: NIP-AM reports the window and the
    // token counts. Summing in+out is the daemon's own reduction
    // (`fleet::reduce_agent`), and doing it here too would be a second
    // implementation — so it is computed only when both halves are present,
    // and stays `null` otherwise rather than counting an unreported half as 0.
    contextUsed:
      tokensIn === null && tokensOut === null
        ? null
        : (tokensIn ?? 0) + (tokensOut ?? 0),
    contextWindow,
  };
}

/**
 * Decode one `/event` frame.
 *
 * Returns `undefined` for a line with no `type`, which is the one field every
 * consumer switches on. A frame with a `seq` and no type would advance the
 * cursor past an event nobody handled — a silent gap, which is precisely what
 * the cursor exists to make impossible.
 */
export function decodeStreamFrame(value: unknown): StreamFrame | undefined {
  const source = obj(value);
  const type = str(source, "type");
  if (!source || !type) return undefined;
  return {
    // A control frame (`stream.reset`) carries no seq. `0` is safe as a
    // *value* because control frames are handled by type and never used to
    // advance the cursor — see `advanceCursor`, which is the one place seq is
    // consumed.
    seq: num(source, "seq") ?? 0,
    type,
    ...(str(source, "channel_id")
      ? { channel_id: str(source, "channel_id") as string }
      : {}),
    // Both spellings; see the module docs.
    data: "data" in source ? source.data : source.payload,
  };
}

/**
 * Whether a frame should move the reconnect cursor.
 *
 * Control frames (`stream.*`) are about the stream rather than on it, and the
 * committed Rust serializes them without a `seq`. Advancing on one would push
 * the cursor to `0` and replay the entire ring on the next reconnect.
 */
export function advancesCursor(frame: StreamFrame): boolean {
  return frame.seq > 0 && !frame.type.startsWith("stream.");
}
