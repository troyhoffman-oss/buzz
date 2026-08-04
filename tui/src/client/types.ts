/**
 * The daemon's wire shapes as the TUI consumes them — `daemon-api.md` Part 3
 * and Part 4, DESIGN.md §2.4.
 *
 * **These are view types, not protocol types.** Every field here is something
 * the daemon has already hydrated: `type: "message.new"`, never `kind: 40002`;
 * `author.name`, never a kind-0 event to parse; `presence: "unknown"`, never a
 * 20001/40902 reconciliation. §6.4's gate exists to keep it that way, and the
 * test of the design is that a `ratatui` front end written against the same
 * daemon loses zero protocol work.
 *
 * TODO(wave1, §4.1.1 deliverable 14): these are hand-written against
 * `daemon-api.md` and are replaced by the **generated** client once
 * `just daemon-spec-check` is wired. Until then this file and the mock daemon
 * in `test/mock-daemon.ts` are the two halves of one contract, and the fixture
 * files under `fixtures/` are what keep them honest.
 */

/** A person or agent, as the daemon hydrates them onto a message. */
export interface Author {
  readonly pubkey: string;
  readonly name: string;
  /** True for managed agents (30177/10100), which changes the presence rules. */
  readonly isAgent: boolean;
}

/**
 * Presence — four states, and `unknown` is never collapsed into `offline`
 * (DESIGN.md §3.1, §2.4).
 *
 * > Rendering `offline` for "I just started and have not heard anything yet" is
 * > exactly the looks-idle-while-the-socket-is-dead failure §1.3 property 3
 * > forbids.
 */
export type Presence = "present" | "waking" | "offline" | "unknown";

/** A reaction group as `GET /message/{id}/reaction` returns it. */
export interface ReactionGroup {
  readonly emoji: string;
  readonly count: number;
  readonly mine: boolean;
}

/** One changed hunk of a diff message, pre-parsed by the daemon. */
export interface DiffHunk {
  readonly oldLine: number | null;
  readonly newLine: number | null;
  readonly kind: "add" | "remove" | "context";
  readonly text: string;
}

/** A diff message's metadata (DESIGN.md §3.1: diffs get their own row). */
export interface DiffMeta {
  readonly path: string;
  readonly added: number;
  readonly removed: number;
  readonly hunks: readonly DiffHunk[];
}

/**
 * A hydrated message.
 *
 * `id` is the event id, and it is what the unread divider anchors to — never a
 * row index (DESIGN.md §3.1, the most-tested piece of chat chrome).
 */
export interface Message {
  readonly id: string;
  readonly channelId: string;
  readonly author: Author;
  /** Unix milliseconds. Rendered through the injected clock, never `Date.now()`. */
  readonly ts: number;
  readonly content: string;
  /** Parent event id when this is a reply. */
  readonly replyTo?: string;
  /** Thread summary counts from the relay overlay (kind 39005). */
  readonly replyCount?: number;
  readonly unreadReplyCount?: number;
  readonly reactions?: readonly ReactionGroup[];
  readonly diff?: DiffMeta;
  /**
   * Non-conversational rows (system, job, huddle-started) render dimmed and are
   * excluded from the unread pill, per `isConversationalUnreadKind`.
   */
  readonly system?: boolean;
  /** Pubkeys mentioned, already resolved by the daemon ([D-2]). */
  readonly mentions?: readonly string[];
}

/** A channel row as `GET /channel` returns it. */
export interface Channel {
  readonly id: string;
  readonly name: string;
  readonly topic?: string;
  readonly unread: number;
  readonly mentions: number;
  /**
   * Agents currently working in this channel — IA §5.3's highest-value ambient
   * signal, which the desktop gives no keyboard reachability at all (IA §6.4).
   */
  readonly agentsWorking: readonly string[];
  readonly starred?: boolean;
  readonly kind?: "channel" | "dm" | "forum";
}

/** Agent working state, as `agent.state` reports it. */
export type AgentState =
  | "working"
  | "needsInput"
  | "failed"
  | "idle"
  | "completed";

/** An agent as `GET /agent` returns it. */
export interface Agent {
  readonly pubkey: string;
  readonly name: string;
  readonly runtime: string;
  readonly presence: Presence;
  readonly state: AgentState;
  /** Channel the current turn is running in, when there is one. */
  readonly channelId?: string;
  readonly channelName?: string;
  readonly turnId?: string;
  /** Turn start, unix ms; the badge anchors to this plus the clock offset. */
  readonly turnStartedAt?: number;
  /** One-line status, e.g. `awaiting permission`. */
  readonly detail?: string;
}

/**
 * A folded transcript row — `GET /agent/{pk}/transcript`.
 *
 * The **folding happens in the daemon**, not here (DESIGN.md §3.4.1): tool
 * start/update pairing, plan replacement, and permission req/resp correlation
 * by JSON-RPC id are a stateful machine that must not be implemented twice.
 */
export interface TranscriptRow {
  readonly id: string;
  readonly ts: number;
  /** Render class from `agentSessionToolClassifier.ts` (§3.4.1). */
  readonly class:
    | "read"
    | "write"
    | "shell"
    | "thought"
    | "plan"
    | "permission"
    | "error"
    | "lifecycle"
    | "message";
  readonly label: string;
  readonly detail?: string;
  /** `+18 −4` for an edit row. */
  readonly added?: number;
  readonly removed?: number;
  readonly running?: boolean;
}

/**
 * Turn usage — kind 44200, rendered with §3.4.1's rules.
 *
 * `null` means **not reported**, not zero: the renderer must show `—`. And
 * `contextWindow` is provider-reported or absent — a client-side model table is
 * precisely the thing that rots, so an absent window renders `ctx 58,204 / —`
 * with **no bar**.
 */
export interface Usage {
  readonly inTokens: number | null;
  readonly outTokens: number | null;
  readonly cacheRead: number | null;
  readonly cacheWrite: number | null;
  readonly costUsd: number | null;
  readonly model: string;
  readonly contextUsed: number | null;
  readonly contextWindow: number | null;
}

/** An attention row on L0 — the inbox filters *are* the groups (§1.3). */
export interface AttentionItem {
  readonly id: string;
  readonly group:
    | "mentions"
    | "threads"
    | "needsAction"
    | "dms"
    | "reminders"
    | "drafts";
  readonly author: string;
  readonly channelName: string;
  readonly channelId: string;
  readonly eventId: string;
  readonly preview: string;
  readonly ts: number;
}

/** A live thread, as the drawer's THREADS section lists it (§2.3). */
export interface LiveThread {
  readonly rootEventId: string;
  readonly channelId: string;
  readonly channelName: string;
  readonly title: string;
  readonly replyCount: number;
  readonly newCount: number;
}

/** A live huddle, as the drawer's HUDDLE section lists it (§2.3). */
export interface Huddle {
  readonly id: string;
  readonly name: string;
  readonly participants: number;
  readonly startedAt: number;
}

/** A community — a row at the top of home, not a layer (§8 ruling 3). */
export interface Community {
  readonly id: string;
  readonly name: string;
  readonly relayUrl: string;
  readonly unread: number;
  readonly active: boolean;
}

/** Connection state, surfaced verbatim to the statusline (`daemon-api.md` §3.1). */
export type ConnectionState =
  | { state: "disconnected" }
  | { state: "connecting" }
  | { state: "authenticating" }
  | { state: "connected" }
  | { state: "rate_limited"; retry_after_ms: number }
  | { state: "reconnecting"; attempt: number; next_retry_in_ms: number }
  | { state: "dns_brownout" }
  | { state: "auth_failed"; reason: string };

/** `GET /session`. */
export interface Session {
  readonly pubkey: string;
  readonly name: string;
  readonly relayUrl: string;
  readonly communityName: string;
  readonly connection: ConnectionState;
  /** False when the daemon runs without an identity — a visible state (§2.5). */
  readonly archiving: boolean;
}

/** A mention-picker candidate — `GET /mention/candidates` (§3.3). */
export interface MentionCandidate {
  readonly pubkey: string;
  readonly handle: string;
  readonly displayName: string;
  readonly isAgent: boolean;
  readonly presence: Presence;
  readonly detail?: string;
  /**
   * Frecency score, `frequency / (1 + ageInDays)`.
   *
   * Ranking lives in the TUI (it is per-front-end personalization, not
   * protocol), but the *candidate* comes from the daemon carrying its pubkey —
   * [D-2]'s guarantee that "what you picked is what gets tagged".
   */
  readonly frecency?: number;
}

/** A search hit — `GET /search` (§3.5). */
export interface SearchHit {
  readonly eventId: string;
  readonly channelId: string;
  readonly channelName: string;
  readonly author: string;
  readonly ts: number;
  readonly excerpt: string;
}

/** The initial snapshot a fixture opens with, and `GET`s the TUI boots from. */
export interface Snapshot {
  readonly session: Session;
  readonly communities: readonly Community[];
  readonly channels: readonly Channel[];
  readonly agents: readonly Agent[];
  readonly attention: readonly AttentionItem[];
  readonly threads: readonly LiveThread[];
  readonly huddles: readonly Huddle[];
  readonly messages: Readonly<Record<string, readonly Message[]>>;
  readonly transcripts: Readonly<Record<string, readonly TranscriptRow[]>>;
  readonly usage: Readonly<Record<string, Usage>>;
  readonly mentionCandidates: readonly MentionCandidate[];
  readonly readMarkers?: Readonly<Record<string, string>>;
}

/**
 * One frame off `GET /event` — `daemon-api.md` §4.2.
 *
 * `seq` is daemon-global and monotonic; it is the reconnect cursor, and the
 * TUI's only job with it is to hand it back on reconnect.
 */
export interface StreamFrame {
  readonly seq: number;
  readonly type: string;
  readonly channel_id?: string;
  readonly data: unknown;
}
