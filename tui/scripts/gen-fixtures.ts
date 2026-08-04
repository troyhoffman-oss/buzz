/**
 * Generate the `fixtures/*.jsonl` scenarios — DESIGN.md §5.4.
 *
 * > A scenario is ordered JSONL of `{atMs, kind, payload}` replayed on the
 * > frozen clock: an initial state snapshot, then the event stream.
 * >
 * > **The same fixture files drive T1, T2, and T3.** That single-source
 * > property is what keeps the tiers from disagreeing.
 *
 * The scenarios are generated rather than hand-written because they must be
 * *internally consistent* — a mention row on home whose `eventId` is not in the
 * channel it names produces a teleport to nothing, and that is a bug the
 * fixture would be teaching rather than catching. Building them from shared
 * constants makes the cross-references structural.
 *
 * Usage: `bun run scripts/gen-fixtures.ts` (regenerates every scenario).
 * The output is committed; `just tui-check-fixtures` fails if it drifts.
 */

import { mkdirSync, writeFileSync } from "node:fs";
import type {
  Agent,
  AttentionItem,
  Channel,
  Community,
  Huddle,
  LiveThread,
  MentionCandidate,
  Message,
  Snapshot,
  TranscriptRow,
  Usage,
} from "../src/client/types";

/**
 * The frozen clock every scenario is authored against — `BUZZ_TUI_FIXED_TIME`.
 *
 * 2026-08-04T14:12:00Z. Every timestamp below is an offset from it, so a
 * snapshot's rendered clock times are stable across machines and time zones
 * (§5.3 determinism requirements 1 and 5).
 */
const T0 = Date.parse("2026-08-04T14:12:00.000Z");
const MIN = 60_000;

/** Stable pubkeys. Opaque to the TUI — never decoded, only compared and hashed. */
const PK = {
  troy: "pk_troy_0000000000000000000000000000000000000000000000000000000001",
  matt: "pk_matt_0000000000000000000000000000000000000000000000000000000002",
  ana: "pk_ana__0000000000000000000000000000000000000000000000000000000003",
  claude1: "pk_cl1__0000000000000000000000000000000000000000000000000000000004",
  goose1: "pk_gse1_0000000000000000000000000000000000000000000000000000000005",
  codex1: "pk_cdx1_0000000000000000000000000000000000000000000000000000000006",
} as const;

const CH = {
  engineering: "ch_engineering",
  buzzDev: "ch_buzz_dev",
  general: "ch_general",
  dmMatt: "dm_matt",
} as const;

const author = (pubkey: string, name: string, isAgent = false) => ({
  pubkey,
  name,
  isAgent,
});

const TROY = author(PK.troy, "troy");
const MATT = author(PK.matt, "matt");
const ANA = author(PK.ana, "ana");
const CLAUDE1 = author(PK.claude1, "claude-1", true);

/**
 * `#engineering` — the channel every walkthrough in NAVIGATION.md §4 uses.
 *
 * Message ids and contents match the spec's frames verbatim where the spec
 * draws them, so a snapshot diff against §4.1–§4.4 is a direct comparison
 * rather than a translation.
 */
const ENGINEERING_MESSAGES: Message[] = [
  {
    id: "ev_eng_001",
    channelId: CH.engineering,
    author: ANA,
    ts: T0 - 52 * MIN,
    content: "bumped the pool ceiling to 24",
    replyCount: 2,
  },
  {
    id: "ev_eng_002",
    channelId: CH.engineering,
    author: TROY,
    ts: T0 - 31 * MIN,
    content: "read-state slots cap at 8 — the 9th write is the interesting one",
    replyCount: 11,
    unreadReplyCount: 1,
  },
  {
    id: "ev_eng_003",
    channelId: CH.engineering,
    author: CLAUDE1,
    ts: T0 - 30 * MIN,
    content:
      "On it. The aux fetch is keyed by the reply id over loaded ids, not by the time window, so a late edit for an old visible message still applies. Two queries, not one.",
    replyTo: "ev_eng_002",
  },
  {
    id: "ev_eng_004",
    channelId: CH.engineering,
    author: CLAUDE1,
    ts: T0 - 29 * MIN,
    content: "session.rs: widen the skew window before the cursor advance",
    diff: {
      path: "crates/buzz-daemon/src/session.rs",
      added: 18,
      removed: 4,
      hunks: [
        {
          oldLine: 86,
          newLine: null,
          kind: "remove",
          text: "let since = last_seen;",
        },
        {
          oldLine: null,
          newLine: 86,
          kind: "add",
          text: "let since = last_seen.saturating_sub(SKEW_SECS);",
        },
        { oldLine: 87, newLine: 87, kind: "context", text: "" },
      ],
    },
  },
  {
    id: "ev_eng_005",
    channelId: CH.engineering,
    author: MATT,
    ts: T0 - 10 * MIN,
    content: "huddle started",
    system: true,
  },
  {
    id: "ev_eng_006",
    channelId: CH.engineering,
    author: MATT,
    ts: T0 - 3 * MIN,
    content:
      "@troy the 44200 cadence is every turn boundary, not every tool call",
    replyCount: 4,
    unreadReplyCount: 2,
    mentions: [PK.troy],
    reactions: [
      { emoji: "♥", count: 2, mine: false },
      { emoji: "💯", count: 1, mine: true },
    ],
  },
  {
    id: "ev_eng_007",
    channelId: CH.engineering,
    author: TROY,
    ts: T0 - 1 * MIN,
    content: "ack — I'll fix the observer to match",
  },
];

const BUZZ_DEV_MESSAGES: Message[] = [
  {
    id: "ev_dev_001",
    channelId: CH.buzzDev,
    author: ANA,
    ts: T0 - 170 * MIN,
    content: "@troy can you look at the pool footprint PR",
    mentions: [PK.troy],
  },
  {
    id: "ev_dev_002",
    channelId: CH.buzzDev,
    author: author(PK.goose1, "goose-1", true),
    ts: T0 - 44 * MIN,
    content: "awaiting permission to write crates/buzz-db/src/read_state.rs",
  },
  {
    id: "ev_dev_003",
    channelId: CH.buzzDev,
    author: MATT,
    ts: T0 - 12 * MIN,
    content: "the lazy pool landed, cold bridges are down to 6",
    replyCount: 3,
  },
];

const GENERAL_MESSAGES: Message[] = [
  {
    id: "ev_gen_001",
    channelId: CH.general,
    author: MATT,
    ts: T0 - 400 * MIN,
    content:
      "read-state is the highest-risk port. contexts map, max-merge, graph-derived parent",
  },
  {
    id: "ev_gen_002",
    channelId: CH.general,
    author: TROY,
    ts: T0 - 390 * MIN,
    content: "agreed. the 9th write is where it gets interesting",
  },
];

/** Replies under `ev_eng_002`, for the L3 THREAD layer of §4.2. */
const THREAD_REPLIES: Message[] = [
  {
    id: "ev_thr_001",
    channelId: CH.engineering,
    author: MATT,
    ts: T0 - 10 * MIN,
    content: "confirmed at exactly 8; the 9th overwrites slot 0",
    replyTo: "ev_eng_002",
  },
  {
    id: "ev_thr_002",
    channelId: CH.engineering,
    author: CLAUDE1,
    ts: T0 - 8 * MIN,
    content:
      "the horizon prune runs first, so slot 0 is usually the oldest context",
    replyTo: "ev_eng_002",
  },
  {
    id: "ev_thr_003",
    channelId: CH.engineering,
    author: TROY,
    ts: T0 - 6 * MIN,
    content:
      "right. and the non-conversational kinds don't count toward unread.",
    replyTo: "ev_thr_001",
  },
];

const CHANNELS: Channel[] = [
  {
    id: CH.engineering,
    name: "#engineering",
    topic: "relay + desktop · agents welcome",
    unread: 8,
    mentions: 1,
    agentsWorking: ["claude-1", "goose-1"],
    kind: "channel",
  },
  {
    id: CH.buzzDev,
    name: "#buzz-dev",
    unread: 3,
    mentions: 0,
    agentsWorking: ["goose-1"],
    kind: "channel",
  },
  {
    id: CH.general,
    name: "#general",
    unread: 0,
    mentions: 0,
    agentsWorking: [],
    kind: "channel",
  },
  {
    id: CH.dmMatt,
    name: "matt",
    unread: 2,
    mentions: 0,
    agentsWorking: [],
    kind: "dm",
  },
];

const AGENTS: Agent[] = [
  {
    pubkey: PK.claude1,
    name: "claude-1",
    runtime: "claude-agent-acp",
    presence: "present",
    state: "working",
    channelId: CH.engineering,
    channelName: "#engineering",
    turnId: "4a91",
    turnStartedAt: T0 - 10 * MIN,
    detail: "turn 4a91",
  },
  {
    pubkey: PK.goose1,
    name: "goose-1",
    runtime: "goose",
    presence: "present",
    state: "needsInput",
    channelId: CH.buzzDev,
    channelName: "#buzz-dev",
    detail: "awaiting permission",
  },
  {
    pubkey: PK.codex1,
    name: "codex-1",
    runtime: "codex",
    presence: "offline",
    state: "idle",
    detail: "last seen 3d",
  },
];

const ATTENTION: AttentionItem[] = [
  {
    id: "att_001",
    group: "mentions",
    author: "matt",
    channelName: "#engineering",
    channelId: CH.engineering,
    eventId: "ev_eng_006",
    preview:
      "@troy the 44200 cadence is every turn boundary, not every tool call",
    ts: T0 - 3 * MIN,
  },
  {
    id: "att_002",
    group: "mentions",
    author: "ana",
    channelName: "#buzz-dev",
    channelId: CH.buzzDev,
    eventId: "ev_dev_001",
    preview: "@troy can you look at the pool footprint PR",
    ts: T0 - 170 * MIN,
  },
  {
    id: "att_003",
    group: "needsAction",
    author: "goose-1",
    channelName: "#buzz-dev",
    channelId: CH.buzzDev,
    eventId: "ev_dev_002",
    preview: "awaiting permission to write crates/buzz-db/src/read_state.rs",
    ts: T0 - 44 * MIN,
  },
  {
    id: "att_004",
    group: "dms",
    author: "matt",
    channelName: "matt",
    channelId: CH.dmMatt,
    eventId: "ev_dm_001",
    preview: "did the upmerge land?",
    ts: T0 - 20 * MIN,
  },
  {
    id: "att_005",
    group: "threads",
    author: "troy",
    channelName: "#engineering",
    channelId: CH.engineering,
    eventId: "ev_eng_002",
    preview: "read-state slots cap at 8",
    ts: T0 - 31 * MIN,
  },
];

const THREADS: LiveThread[] = [
  {
    rootEventId: "ev_eng_002",
    channelId: CH.engineering,
    channelName: "#engineering",
    title: "read-state slots",
    replyCount: 11,
    newCount: 1,
  },
  {
    rootEventId: "ev_eng_006",
    channelId: CH.engineering,
    channelName: "#engineering",
    title: "44200 cadence",
    replyCount: 4,
    newCount: 2,
  },
];

const HUDDLES: Huddle[] = [
  { id: "hud_001", name: "standup", participants: 3, startedAt: T0 - 8 * MIN },
];

const COMMUNITIES: Community[] = [
  {
    id: "com_block",
    name: "block",
    relayUrl: "buzz://relay.example",
    unread: 12,
    active: true,
  },
  {
    id: "com_fork",
    name: "fork",
    relayUrl: "buzz://fork.example",
    unread: 0,
    active: false,
  },
];

const TRANSCRIPT: TranscriptRow[] = [
  {
    id: "tr_001",
    ts: T0 - 10 * MIN,
    class: "lifecycle",
    label: "turn started",
  },
  {
    id: "tr_002",
    ts: T0 - 10 * MIN + 1000,
    class: "thought",
    label: "thought",
    detail:
      "Need to check how aux events are fetched before touching the window query.",
  },
  {
    id: "tr_003",
    ts: T0 - 9 * MIN,
    class: "read",
    label: "read  crates/buzz-daemon/src/session.rs",
    detail: "lines 1-120",
  },
  {
    id: "tr_004",
    ts: T0 - 8 * MIN,
    class: "shell",
    label: "shell cargo check -p buzz-daemon",
    detail: "exit 0 · 3.4s",
  },
  {
    id: "tr_005",
    ts: T0 - 7 * MIN,
    class: "write",
    label: "edit  crates/buzz-db/src/read_state.rs",
    added: 18,
    removed: 4,
  },
  {
    id: "tr_006",
    ts: T0 - 6 * MIN,
    class: "shell",
    label: "test  read_state::slot_overflow",
    detail: "ok",
  },
  {
    id: "tr_007",
    ts: T0 - 1 * MIN,
    class: "shell",
    label: "bash  cargo test -p buzz-db",
    detail: "running",
    running: true,
  },
];

/**
 * Usage with a **null** cache-write and a **null** context window.
 *
 * Deliberate: §3.4.1 makes `null` mean *not reported* and requires `—` rather
 * than `0`, and makes an absent context window render `ctx N / —` with no bar.
 * A fixture that reported every field would leave both rules untested, which is
 * how the draft mock ended up violating them.
 */
const USAGE: Usage = {
  inTokens: 12480,
  outTokens: 1932,
  cacheRead: 9120,
  cacheWrite: null,
  costUsd: 0.0412,
  model: "claude-opus-5",
  contextUsed: 58204,
  contextWindow: null,
};

const MENTION_CANDIDATES: MentionCandidate[] = [
  {
    pubkey: PK.matt,
    handle: "matt",
    displayName: "Matt",
    isAgent: false,
    presence: "present",
    frecency: 4.2,
  },
  {
    pubkey: PK.troy,
    handle: "troy",
    displayName: "Troy",
    isAgent: false,
    presence: "present",
    frecency: 3.1,
  },
  {
    pubkey: PK.ana,
    handle: "ana",
    displayName: "Ana",
    isAgent: false,
    presence: "unknown",
    frecency: 1.4,
  },
  {
    pubkey: PK.claude1,
    handle: "claude-1",
    displayName: "claude-agent-acp",
    isAgent: true,
    presence: "present",
    detail: "2 turns",
    frecency: 5.0,
  },
  {
    pubkey: PK.goose1,
    handle: "goose-1",
    displayName: "goose",
    isAgent: true,
    presence: "waking",
    frecency: 2.0,
  },
  {
    pubkey: PK.codex1,
    handle: "codex-1",
    displayName: "codex",
    isAgent: true,
    presence: "offline",
    detail: "last seen 3d",
    frecency: 0.2,
  },
];

const SEEDED_SNAPSHOT: Snapshot = {
  session: {
    pubkey: PK.troy,
    name: "troy",
    relayUrl: "buzz://relay.example",
    communityName: "block",
    connection: { state: "connected" },
    archiving: true,
  },
  communities: COMMUNITIES,
  channels: CHANNELS,
  agents: AGENTS,
  attention: ATTENTION,
  threads: THREADS,
  huddles: HUDDLES,
  messages: {
    [CH.engineering]: [...ENGINEERING_MESSAGES, ...THREAD_REPLIES],
    [CH.buzzDev]: BUZZ_DEV_MESSAGES,
    [CH.general]: GENERAL_MESSAGES,
    [CH.dmMatt]: [
      {
        id: "ev_dm_001",
        channelId: CH.dmMatt,
        author: MATT,
        ts: T0 - 20 * MIN,
        content: "did the upmerge land?",
      },
    ],
  },
  transcripts: { [PK.claude1]: TRANSCRIPT },
  usage: { [PK.claude1]: USAGE },
  mentionCandidates: MENTION_CANDIDATES,
  /**
   * The unread divider anchor — an **event id**, never a row index (§3.1).
   *
   * `ev_eng_004` means four messages in `#engineering` are new, matching the
   * channel row's `unread: 8` only loosely on purpose: unread *counts* come
   * from the daemon's own recompute and the *divider* comes from the read
   * marker, and a fixture where the two agree by construction would never
   * catch a renderer that derived one from the other.
   */
  readMarkers: { [CH.engineering]: "ev_eng_004" },
};

/** One line of a scenario file (§5.4). */
interface FixtureLine {
  readonly atMs: number;
  readonly kind: string;
  readonly payload: unknown;
}

function write(name: string, lines: readonly FixtureLine[]): void {
  mkdirSync(new URL("../fixtures/", import.meta.url), { recursive: true });
  const path = new URL(`../fixtures/${name}.jsonl`, import.meta.url).pathname;
  writeFileSync(path, `${lines.map((l) => JSON.stringify(l)).join("\n")}\n`);
  console.log(`wrote fixtures/${name}.jsonl (${lines.length} lines)`);
}

// ── scenarios ───────────────────────────────────────────────────────────────

/** `seeded-basic` — 4 channels, ~15 messages, unread + mentions (§5.4). */
write("seeded-basic", [
  { atMs: 0, kind: "snapshot", payload: SEEDED_SNAPSHOT },
]);

/** `empty` — cold start, no communities. The zero-state every list must handle. */
write("empty", [
  {
    atMs: 0,
    kind: "snapshot",
    payload: {
      session: {
        pubkey: PK.troy,
        name: "troy",
        relayUrl: "buzz://relay.example",
        communityName: "block",
        connection: { state: "connecting" },
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
    } satisfies Snapshot,
  },
]);

/**
 * `agent-stream` — a live turn advancing while the drawer is open (§4.3).
 *
 * The frames land at 400 ms intervals so a T2 run can observe the tail
 * advancing *while reading*, which is the property §2.4 claims and the one an
 * all-at-once fixture cannot demonstrate.
 */
write("agent-stream", [
  { atMs: 0, kind: "snapshot", payload: SEEDED_SNAPSHOT },
  {
    atMs: 400,
    kind: "agent.frame",
    payload: {
      agentPubkey: PK.claude1,
      row: {
        id: "tr_008",
        ts: T0 + 400,
        class: "shell",
        label: "test  read_state::slot_overflow",
        detail: "ok",
      },
    },
  },
  {
    atMs: 800,
    kind: "agent.frame",
    payload: {
      agentPubkey: PK.claude1,
      row: {
        id: "tr_009",
        ts: T0 + 800,
        class: "write",
        label: "edit  crates/buzz-db/src/read_state.rs",
        added: 3,
        removed: 1,
      },
    },
  },
  {
    atMs: 1200,
    kind: "agent.metric",
    payload: {
      agentPubkey: PK.claude1,
      usage: { ...USAGE, inTokens: 13890, outTokens: 2210 },
    },
  },
  {
    atMs: 1600,
    kind: "message.new",
    payload: {
      id: "ev_eng_008",
      channelId: CH.engineering,
      author: CLAUDE1,
      ts: T0 + 1600,
      content:
        "read_state slot overflow now covered; 9th write overwrites slot 0 as expected",
    },
  },
]);

/** `reconnect` — a loss state and its recovery, for the statusline (§2.6). */
write("reconnect", [
  { atMs: 0, kind: "snapshot", payload: SEEDED_SNAPSHOT },
  {
    atMs: 500,
    kind: "connection.state",
    payload: { state: "reconnecting", attempt: 2, next_retry_in_ms: 4000 },
  },
  { atMs: 2000, kind: "connection.state", payload: { state: "connected" } },
]);

/**
 * `keyless-daemon` — `archiving: false` surfaced in chrome (§2.5).
 *
 * A keyless daemon must never look identical to a healthy one, and this is the
 * fixture that makes the difference assertable.
 */
write("keyless-daemon", [
  {
    atMs: 0,
    kind: "snapshot",
    payload: {
      ...SEEDED_SNAPSHOT,
      session: { ...SEEDED_SNAPSHOT.session, archiving: false },
    } satisfies Snapshot,
  },
]);
