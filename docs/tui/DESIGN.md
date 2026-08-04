# Buzz TUI — Design

Status: design of record. Supersedes nothing; consumes
`feature-inventory.md` (what parity means), `daemon-api.md` (the boundary),
`ux-patterns.md` (how it should feel), and the synthesis section of
`../tui-research.md` (why this stack).

Decisions marked **[LOCKED]** came from the owner and are not open for
relitigation here. Decisions marked **[D-n]** are made by this document and are
the things to argue with.

---

## 1. Vision and non-goals

### 1.1 The one-sentence version

A terminal client for Buzz that a terminal-native operator would choose over the
desktop app — not a degraded fallback — built so the protocol work is permanent
and the rendering layer is disposable.

### 1.2 Why it exists

Buzz's remote-first thesis is that agents live on VPSes. Its two design partners
live there too: both operators run long-lived `tmux` sessions on their own boxes,
and reach them from a phone through Moshi terminal mirroring. Today that means
the surface they use to *supervise* agents is a desktop app on a different
machine from the agents, while the machine that hosts the agents has no Buzz
client at all. The TUI closes that gap: the client runs where the work runs.

Two consequences fall straight out and shape everything below:

- **The phone story is free and must not be designed for separately.** Moshi
  mirrors a `tmux` pane. A layout that degrades correctly at 50 columns *is* the
  mobile client. This is why the XS responsive tier (ux-patterns P25) is a
  first-class requirement in Wave 1 and not a nice-to-have — it is one of the two
  operators' primary reading surfaces.
- **Agent supervision is the flagship surface, not chat.** The observer transcript
  (kind 24200) and NIP-AM turn metrics (kind 44200) are text, structured,
  high-volume, and currently under-rendered even on the desktop — 44200 is
  archived and never displayed anywhere. A terminal is the correct medium for
  them, and the TUI can be *better* than the desktop here on day one rather than
  chasing parity.

### 1.3 What "good" means

The bar is **"whatever the Buzz team would do natively"** [LOCKED]. Concretely,
five properties, each of which is a review gate later in this document:

1. **Invisible infrastructure.** The user runs `buzz-tui`. There is no
   "start the daemon first", no port to pick, no config file to author before
   first use. The daemon is an implementation detail that happens to survive
   Ctrl-C.
2. **No dead ends.** Every capability that cannot work in a terminal has an
   explicit, one-keystroke handoff to the desktop with the exact deep link —
   never a broken imitation, never silence. (§4.6)
3. **Loss is always visible.** Inherited verbatim from the harness's design
   philosophy: dropped observer frames, gapped event cursors, unreachable relay,
   ambiguous moderation writes. A chat client that *looks* idle while its socket
   is dead is the worst failure mode in this product, and the desktop learned
   that the hard way (`rate_limited`, `dns_brownout` are real user-visible
   states, not internal ones).
4. **One definition, three surfaces.** Every command is reachable by key, by
   palette, and by slash, from a single declarative table (ux-patterns P1/P3/P21).
   No hand-written palette list, no hardcoded keys in handlers.
5. **Full desktop parity is the destination** [LOCKED], shipped in waves ordered
   by pain, not by module size.

### 1.4 Non-goals

| Non-goal | Why |
|---|---|
| **A separate mobile client or mobile-specific UI** | Moshi + `tmux` already delivers it. A second path would be two codebases for one user. |
| **Image, video, or avatar rendering** | Sixel/kitty graphics is a per-terminal capability lottery. All image surfaces are desktop handoffs (§4.6). Revisit only after parity. |
| **Huddle audio** | Real-time audio in the process that also owns the chat session is a reliability trade we will not make. Read-only huddle state + join-in-desktop. |
| **An offline write queue** | Writes go to the relay or fail visibly. Reconciling queued writes against NIP-33 last-writer-wins replaceables is a distributed-systems project, not a chat feature. The composer holds unsent text with an explicit retry instead (§2.7). |
| **A second auth model** | No API tokens, no daemon users. Filesystem permissions on the socket plus a peer-credential check are the authorization boundary. |
| **Multi-community aggregation inside the daemon** | One daemon per (relay, identity). The TUI opens N and merges. This makes the desktop's `resetCommunityState()` hazard structurally impossible rather than a checklist. |
| **Being a git client** | `git` is better than what we would write. NIP-34 events read like any other events; packfiles never touch the daemon. |
| **A pretty front end that owns protocol knowledge** | The front end is deliberately disposable [LOCKED]. §6.4 makes that a CI gate rather than an intention. |

### 1.5 Users

Troy and Matt. Both terminal-native, both on VPSes, both in long-lived `tmux`,
both reaching those sessions from a phone via Moshi. Two users is small enough
that the design can be opinionated (one leader key, one theme family, no
preference sprawl) and large enough that the second-client-attach case is real
from day one — two panes, two clients, one daemon, one relay budget.

---

## 2. Architecture

### 2.1 Process model

```
  ┌── tmux pane 1 ──────────────┐        ┌── tmux pane 2 ─────────┐
  │  buzz-tui                   │        │  buzz-tui --attach     │
  │  Bun + OpenTUI + Solid      │        │  (or `buzz watch`)     │
  │  ── no key, no nostr, ──    │        │                        │
  │     no relay URL            │        │                        │
  └──────────────┬──────────────┘        └───────────┬────────────┘
                 │  HTTP/1.1 + ndjson over UDS       │
                 │  $XDG_RUNTIME_DIR/buzz/<hash>.sock (0600)
                 └───────────────┬───────────────────┘
                                 ▼
            ┌────────────────────────────────────────────┐
            │  buzz-daemon (Rust)                        │
            │   • key custody (nsec, NIP-44, NIP-98)     │
            │   • relay session: NIP-42, subs, reconnect │
            │   • cache: channels, roster, profiles,     │
            │     read-state, reactions, observer archive│
            │   • observer decrypt + 9-guard chain       │
            │   • backend-provider conduit               │
            │   uses buzz-ws-client + buzz-sdk VERBATIM  │
            └───────────────┬───────────────┬────────────┘
                            │               │
              WebSocket     │               │  one-shot subprocess
              NIP-42 + HTTP │               │  (stdin JSON → stdout JSON)
                            ▼               ▼
                      ┌──────────┐   ┌──────────────────┐
                      │  relay   │   │ buzz-backend-ssh │
                      └──────────┘   │ buzz-backend-k8s │
                                     └──────────────────┘
```

**[LOCKED]** Stack is OpenTUI/TypeScript front end over a small Rust
`buzz-daemon` that reuses `buzz-ws-client` and `buzz-sdk` verbatim. The daemon
owns **all** protocol knowledge.

The invariant that makes the front end disposable: the TUI's entire network
layer is one HTTP client and one line reader. It never parses a Nostr event,
never sees a key, never knows a relay URL, and never learns an event kind
number. §6.4 turns that from a rule into a CI gate.

### 2.2 One daemon per (relay, identity)

Socket path: `$XDG_RUNTIME_DIR/buzz/<hash>.sock` on Linux,
`~/Library/Application Support/buzz/run/<hash>.sock` on macOS, where
`<hash> = sha256(relay_url + ":" + pubkey + ":" + auth_tag_owner)[0..16]` and
`auth_tag_owner` is the owner pubkey from the NIP-OA auth tag, or `""` when
there is none.

**The auth tag is part of the identity, not decoration.** Under NIP-OA the
*effective* identity is (agent pubkey, owner attestation): the same key with and
without a `BUZZ_AUTH_TAG`, or with two different tags, has different relay
permissions and different reachable channels. Omitting it from the preimage
would collide two distinct effective identities onto one socket, one cache, and
one read-state slot. The **full preimage is stored in the pidfile JSON** so a
support conversation can identify a collision by reading it rather than by
guessing.

Multi-community is an N-daemon fan-out in the TUI, not multi-tenancy in the
daemon. The community rail switches which daemon handle is active. Because each
daemon is a separate process with a separate cache and a separate relay budget,
the entire class of bug that `resetCommunityState()` exists to prevent on the
desktop — module-level caches leaking across a relay boundary — cannot occur.
There is no shared memory to leak through.

Cost: N daemons for N communities, and this fork has already been burned once by
an unbounded per-process estimate (48 cold ACP bridges at ~2.8 GB). So the bound
is mechanical, not estimated:

- **A hard cap of 10 live daemons per user.** The 11th spawn fails with
  `daemon_limit_reached` naming the remedy: `buzz-tui daemon list`.
- **`buzz-tui daemon list` and `daemon stop --all`** are Wave-1 surfaces
  (`GET /daemon/registry` reads a `0700` registry directory of pidfiles; each
  daemon's `/daemon/shutdown` does the stopping). `/daemon/shutdown` without an
  enumeration path is a leak with no broom.
- **The idle timer keys on client *activity*, not on connection presence.** A
  detached `tmux` pane holding an `/event` stream open is the normal state, not a
  live client; a stream with no request in `--idle-timeout` counts as idle and
  the daemon exits. Without this the default 30-minute timer never fires for the
  exact population this product targets.
- **Wave-1 exit criterion 5 counts daemons and total RSS**, not just drop
  counters (§4.1.4).

With that bound in place the isolation is worth strictly more than the RSS.

### 2.3 Daemon lifecycle

The user never types `buzz-daemon` [LOCKED]. Spawn is a TUI startup step:

```
buzz-tui start
 ├─ resolve identity → resolve socket path
 ├─ connect(socket)
 │   ├─ ok → GET /health → api_version floor check → attach  (fast path, ~1 ms)
 │   └─ ENOENT | ECONNREFUSED
 │        ├─ flock(<hash>.lock)                # two TUIs racing → one spawns
 │        ├─ connect(socket) AGAIN             # ← load-bearing: the winner may
 │        │    └─ ok → unlock → attach         #   have spawned while we blocked
 │        ├─ liveness-probe the socket, not the pid:
 │        │    connect() refused/ENOENT = dead;  connected-but-erroring = alive
 │        │    (a pidfile pid check is a PID-reuse race and is not used)
 │        ├─ unlink socket only on confirmed-dead
 │        ├─ materialize embedded daemon binary if absent (§6.3)
 │        ├─ spawn buzz-daemon --socket <path> --detach
 │        │       --identity-ncryptsec <path> --passphrase-stdin
 │        ├─ write the PASSPHRASE to the child's stdin, close it   (§2.5)
 │        ├─ poll connect() @25 ms, ≤5 s
 │        │    └─ terminate early on child exit; surface the child's
 │        │       redacted stderr, never "timed out"
 │        └─ unlock
 └─ GET /session → identity, relay, connection state → first paint
```

**The post-lock re-`connect()` is the whole point of the lock.** Without it the
loser of the race acquires the lock *after* the winner has released it and
proceeds straight to unlink-and-spawn — destroying a live socket and producing
two daemons on one (relay, identity). That is not a cosmetic duplicate: two
NIP-42 sessions, two relay-budget consumers, and two read-state publishers
racing on the same 30078 `d` coordinate. The desktop already carries explicit
conflict detection for exactly this (`readStateManager.ts` rotates `slotId` when
another `client_id` squats its `d`-tag), which is the evidence that the hazard
is real. T2 asserts it: two TUIs launched within 50 ms yield exactly one daemon
pid (§5.5).

**Spawn failure is a first-class outcome**, not a timeout. A daemon that exits
immediately — wrong passphrase, corrupt ncryptsec, `EADDRINUSE` — must surface
its own (redacted) stderr. "Timed out after 5 s" for a bad passphrase is the
kind of dead end §1.3 property 2 forbids.

Lifetime rules:

- **Never exits on last-client-disconnect.** An agent turn may be streaming
  frames the operator wants when they come back to the pane. The daemon keeps
  subscribing and archiving.
- **Idle shutdown** after `--idle-timeout` (default 30 min with no client).
  `--idle-timeout 0` disables it — and the VPS install (§6.5) sets `0`, because
  on the agent host the daemon *is* the always-on archive.
- **Version skew is a compatibility floor plus capability presence — one
  mechanism, not two. [D-1]** `/health` returns
  `{version, api_version, capabilities[]}`. The rule is
  `daemon.api_version >= client.min_api_version`; below the floor is a hard
  error printing
  `daemon 0.4.1 (api 3) is running; this client needs api ≥4 — upgrade the daemon`.
  At or above the floor, `capabilities[]` (implemented API groups: `channels`,
  `agents`, `projects`, …) decides which screens exist, so a Wave-3 TUI attached
  to a Wave-1 daemon on a remote box *hides* what it cannot serve rather than
  erroring inside it.

  An earlier draft said "`api_version` must match exactly; version skew is never
  a negotiation" *and* kept `capabilities[]`. Those cannot both hold —
  capabilities **are** negotiation — and exact-match is actively harmful in the
  install shape this product is built for: on the VPS the daemon runs under
  `systemd --user` with `--idle-timeout 0`, so "restart the daemon to match your
  client" means **killing the always-on observer archive**, which is the entire
  reason that install shape exists (§6.5). The floor keeps the archive alive.

  What makes the floor safe is a contract, not politeness: **API changes are
  additive-only**, enforced by `daemon-spec-check` (§6.2) — a removed or narrowed
  endpoint fails the build. A genuine breaking change is an `api_version` bump,
  which is precisely what the floor is for.

  `buzz-tui daemon restart` must **detect a systemd-managed daemon** (marker
  written into the pidfile at spawn) and print
  `systemctl --user restart buzz-daemon` rather than SIGTERM-ing a unit systemd
  will resurrect underneath it.
- **Crash** → the TUI's ndjson stream errors → status bar shows
  `daemon lost — respawning` → the handshake re-runs → the cache reloads from
  SQLite, not from the relay.

### 2.4 API boundary — refinements to `daemon-api.md`

The shape in `daemon-api.md` is adopted wholesale: one base URL, one event
stream, an OpenAPI 3.1 document, a generated client, auto-spawn with attach.
What follows is only where this document changes or resolves it.

**[D-2] The TUI sends resolved pubkeys, never names.** `daemon-api.md` §3.3
allows `POST /channel/{id}/message` to omit `mentions` and have the daemon run
`extract_at_mentions_with_known` server-side. That path stays for `curl` and for
second clients, but the TUI **must not use it**. `GET /mention/candidates`
returns candidates that already carry their pubkey; the composer's parts array
(ux-patterns P10) holds the pubkey; the send carries an explicit `mentions:
[pubkey…]`. Two payoffs: "what you picked is what gets tagged" becomes true by
construction rather than by two implementations agreeing, and **frecency ranking
can live in the TUI** (where it belongs — it is per-front-end UI
personalization, not protocol) without any risk of the ranked pick and the
resolved tag diverging.

`MENTION_CAP = 50` (`crates/buzz-sdk/src/mentions.rs:38`) is a **build-time**
rejection in the SDK (`SdkError::TooManyMentions`), so it must be surfaced
before the send, not after. The daemon returns `400 too_many_mentions {cap: 50,
requested: n}`, and the composer shows a live `n of 50` counter at pick time —
failing at Enter on a message the operator has already written is the worst
possible place to learn about a cap.

**`@channel` is deferred out of Wave 1.** The §3.3 mock previously showed
`@channel — notify 27 members`. There is no wire representation for it anywhere:
no handling in `buzz-sdk`, none in the relay, none in `desktop/src`. Client-side
expansion to N `p` tags works at 27 members and hard-fails at 51, which makes it
a feature that breaks as a community grows. It returns when it is specified as
protocol (a marker tag the relay expands, with fan-out accounted at the relay),
not before. Tracked as Q8 (§7.2).

**[D-3] Observer frames are stored as ciphertext at rest.** Resolves
`daemon-api.md` open question 1. Kind-24200 payloads are the richest plaintext
on the box: full prompts, system prompts, file contents, shell output, tool
arguments. Decrypt is a pure function of (owner key, event), so storing
ciphertext costs one ECDH per read and nothing else. The SQLite file is still
`0600` in a `0700` directory; ciphertext-at-rest is defense in depth, not a
substitute for that.

**The decrypted-frame cache ports the desktop's *separation*, not its number.**
`observerRelayStore.ts` keeps two structures on purpose: a live ring capped at
`MAX_OBSERVER_EVENTS = 3000` per agent, and a **distinct channel-scoped archive
journal** that grows only by explicit paged loads from SQLite — "strict
separation so loading deep history can never evict live frames." Collapsing both
into one LRU inherits neither property, and the arithmetic is worse than it
looks: 32 concurrent agents (`BUZZ_ACP_AGENTS` is `1..=32`) × 3000 frames ×
`OBSERVER_MAX_PLAINTEXT_LEN` 65,535 bytes is a ~6 GB worst case. So:

- **Live ring**: per-agent, 3000 frames, matching the desktop.
- **Archive reads stream from SQLite** and never populate the live ring.
- **The decrypt cache is sized in bytes, not frames** — a single global budget
  (default 256 MB, `--observer-cache-bytes`) evicted LRU. Frame *count* is not a
  memory bound when a frame carries an arbitrary-size `payload`.
- **Unknown-agent frames are queued, not dropped.** Guard 4 (§2.5) rejects a
  frame whose pubkey is not in the agent registry — but on cold start the
  registry has not loaded yet, and dropping there is the difference between "the
  agent feed works" and "the agent feed is empty until you restart." The desktop
  buffers up to `MAX_PENDING_UNKNOWN_AGENT_FRAMES = 100` pending frames and
  re-evaluates them when the registry arrives; the daemon does the same, with
  its own counter for frames evicted from that queue.

The hot path — the activity viewer scrolling live frames — therefore never
re-decrypts, and deep history paging pays the ECDH without touching the ring.

**[D-4] The daemon implements the session constant table itself; it does not
fork or prematurely extract `buzz-acp::relay`.** Resolves open question 2, and
this is the one place where a duplication is being accepted on purpose, so the
reasoning is stated in full.

`relay.rs` is 6,321 lines under active upstream development. Three options:

- *Fork it* — permanently, and inherit every upstream fix by hand. Rejected.
- *Extract `buzz-session` now* — a large refactor of an actively-developed
  upstream file, in a fork that must keep upmerging. Every upstream touch of
  `relay.rs` becomes a conflict, forever, before we know whether the abstraction
  is right (one consumer cannot tell you).
- *Reimplement the policy* — smaller than the refactor, zero upmerge risk,
  directly testable. **But the load-bearing part is the state machine, not the
  constant table**, and an earlier draft's "~600 lines" estimate understated it.
  The constants (all thirteen verified at `relay.rs:29–112`) are the cheap half.
  The expensive half is the recovery behaviour: `gated_observer_pending` with
  drop-oldest and `gated_observer_dropped` accounting; `observer_in_flight` plus
  `requeue_observer_in_flight()`, which restores unacked writes **ahead** of
  newly parked frames because a NOTICE carries no event id and every unacked
  frame must therefore be conservatively retried; `acknowledge_observer_frame`;
  the rate-limit gate arm/disarm machine; the `n_sub_active` /
  `observer_control_sub_active` resubscribe-on-reconnect flags; per-channel
  `last_seen` replay; the two-generation membership dedup with its strict-`<`
  watermark; and a `pacing_sleep` that stays shutdown-aware while deferring
  commands. Realistic size: **~1,500–2,000 lines**. Copying the constants
  verbatim proves nothing about any of that, which is why §5.2 names those five
  recovery behaviours as their own T0 cases.

Chosen: reimplement. **This duplication is time-boxed and has a named exit
criterion**: once `buzz-daemon` has run in production through at least one
relay-side incident (rate limiting, DNS brownout, or a service restart), the
pure-policy half — backoff ladder, `TwoGenDedup`, REQ pacing, the gated-observer
queue, `since`-watermark resubscribe — is extracted into a no-I/O
`buzz-relay-session` crate and both consumers move onto it, as an
*upstream-submittable* PR. Two consumers first, then extract. If that follow-up
has not shipped one wave after Wave 1, it becomes a blocker on Wave 3, not a
backlog item.

The constants are copied verbatim, not re-derived: `SEEN_ID_LIMIT` 12,000 ·
`PING_INTERVAL` 30 s / `PONG_TIMEOUT` 10 s · `STABLE_CONNECTION_SECS` 60 ·
`SINCE_SKEW_SECS` 5 · `STARTUP_CONNECT_BACKOFFS` 1,2,4,8,16 s ·
`DNS_RETRY_INTERVAL` 2 s ±20% · `REQ_PACING_INTERVAL` 125 ms ·
`DRAIN_BUDGET_PER_ITER` 1 · `GATED_OBSERVER_QUEUE_CAP` 256. Every one of them
gets a comment naming `crates/buzz-acp/src/relay.rs` as its origin, so the
extraction PR can prove equivalence rather than argue it.

**[D-5] Per-topic drop policy on `/event`, not disconnect-on-overflow.**
Resolves open question 4. `daemon-api.md` proposes killing a slow consumer.
That is honest but wrong for a TUI that legitimately blocks for a second while
rendering a large diff. The event stream inherits the harness's
ephemeral-vs-durable split instead:

| Class | Topics | Policy when the client's buffer backs up |
|---|---|---|
| **Durable** | `message.*`, `thread.reply`, `agent.frame`, `agent.metric`, `agent.permission.request`, `read_state.update`, `channel.member`, `connection.state` | Never dropped. Park in order. If the park queue exceeds its cap, emit `stream.overflow{dropped, since_seq}` and then disconnect — loss is announced before it happens. |
| **Coalescing** | `presence.update`, `typing.start`, `channel.unread`, `agent.state` | Latest-wins per key. Dropping an intermediate value is semantically free because the next one supersedes it. |

This is the same distinction `relay.rs` already makes between typing (dropped
under the rate-limit gate) and observer telemetry (parked and paced). Making the
daemon→TUI hop obey the same rule means one mental model end to end.

**[D-6] Cursors are versioned, decodable, and not parsed.**
`c1.<base64url({"until":…,"before_id":…})>`. The `c1.` prefix makes a future
cursor format a clean rejection rather than a mis-parse; base64url-of-JSON makes
a support conversation a `base64 -d` away. The TUI treats it as opaque; a CI
test asserts the TUI never decodes one.

**[D-7] `local_id` is a daemon-side map only.** Confirmed from open question 5 —
the daemon computes the event id at sign time, so correlating the relay `OK`
back to the client's provisional id needs no wire change and **no tag on the
event**. Stated explicitly so nobody adds one.

**[D-10] The timeline is fetched as a NIP-CW server-assembled window, and
`kind:39006` is the only exhaustion authority.** An earlier draft specified a
two-query fetch (content kinds by time window, aux kinds by `#e`) and called it
"verbatim." It is not verbatim — it is the *pre*-NIP-CW desktop path, and
reimplementing it reintroduces exactly the bugs NIP-CW exists to remove. The
current desktop sends `top_level: true`, `include_summaries: true`,
`include_aux: true`, plus `(until, before_id)`
(`desktop/src-tauri/src/commands/channel_window.rs`). Three things the two-query
plan loses:

- A plain `kinds` + `#h` filter **cannot express "not a reply"**, so `limit`
  counts raw events: a page of 50 may contain 3 top-level rows or 50.
- `39006` carries the authoritative `has_more`. NIP-CW §Client Behavior is
  explicit — *"A client MUST NOT stop paging on row count"*; an exact-multiple
  final page returns `limit` rows with `has_more: false`. The obvious
  implementation of the `{next, has_more}` contract (short page = done) is the
  precise bug the NIP forbids.
- Thread summaries (`kind:39005`) arrive as relay-signed overlays with the
  window, and are the cheap source for §3.1's `⤷ 4` reply count.

Wave 1 therefore implements: the window filter above; **bounds-integrity checks
per NIP-CW §Client Behavior step 5** — exactly one `39006`, its `d`-tag binding
echoes the request cursor, content parses, and `has_more = true ⇔ next_cursor ≠
null`; anything else means **discard the page and retry, never guess**; overlays
are metadata and never render as rows or feed cursor math. The **degradation
branch is also Wave 1**: no valid `39006` (extension-unaware relay, or a strict
parser rejecting the filter) → reissue a clean standard filter with the
extension keys removed and assemble threads client-side, which is what
`auxBackfill.ts` still does for threads. Downgrade is a decision, not a fallback
that happens by accident.

**Global daemon invariant: no filter leaves the daemon without an explicit
`kinds`.** This is not a search rule — §3.5 previously stated it as one, which
would mislead an implementer into setting `kinds` on `/search` and forgetting it
on `/message/{id}`. The actual gate is `p_gated_filters_authorized`
(`crates/buzz-relay/src/handlers/req.rs` → `crates/buzz-core/src/kind.rs`): a
filter that *can match* any `P_GATED_KIND` is refused unless its `#p` values
equal the authenticated reader's pubkey — and a filter with **no** `kinds` can
match everything. It applies to `REQ` and `/query` equally, closing as
`restricted:` or `403` depending on transport. `P_GATED_KINDS` today:
`KIND_AGENT_OBSERVER_FRAME` (24200), `KIND_MEMBER_ADDED_NOTIFICATION`,
`KIND_MEMBER_REMOVED_NOTIFICATION`, `KIND_GIFT_WRAP`, `KIND_DM_VISIBILITY`
(30622), `KIND_AGENT_TURN_METRIC` (44200).

The `ids` exemption has two carve-outs that bite a Wave-1 endpoint:
`RESULT_GATED_KINDS = [KIND_DM_VISIBILITY, KIND_AGENT_TURN_METRIC]` lose the
exemption when named explicitly. So `/agent/{pk}/metric` **must** carry
`#p = self`; an `{ids: […], kinds: [44200]}` lookup is refused. Specified here
because the endpoint would otherwise ship 403-ing.

**Presence has two sources and three states.** Kind 20001 is *ephemeral* —
`is_ephemeral(20001)` is true and the relay never stores it — so a daemon that
starts after an agent's last beat sees nothing until the next one. Durable
last-seen comes from **kind 40902** (`KIND_PRESENCE_SNAPSHOT`), which is in the
Wave-1 source set. The rule: live presence from 20001; cold-start and last-seen
from 40902; and an explicit **`unknown`** state distinct from `offline` for the
window before the first beat. Rendering `offline` for "I just started and have
not heard anything yet" is exactly the looks-idle-while-the-socket-is-dead
failure §1.3 property 3 forbids.

**Wave-1 endpoint subset.** The daemon is built to the wave, not to the whole
spec. Wave 1 implements exactly:

```
/health  /openapi.json  /daemon  /daemon/registry  /daemon/shutdown
/daemon/reconnect
/session  /session/identity  /session/relay-info
/channel  /channel/{id}  /channel/{id}/member  /channel/{id}/join|leave
/channel/{id}/message  /channel/{id}/typing  /channel/{id}/read
/message/{id}  /message/{id}/thread  /message/{id}/reaction
/message/{id}/ask                       # answer an ask card (threaded reply)
/search  /search/user
/user  /user/{pubkey}  /mention/candidates  /mention/inbox
/read-state  /presence
/agent  /agent/{pk}  /agent/{pk}/activity  /agent/{pk}/transcript
/agent/{pk}/metric  /agent/{pk}/control  /agent/fleet
/event
```

**`POST /agent/{pk}/control` accepts exactly two payload types in Wave 1:
`cancel_turn` and `switch_model`.** This is not a scoping preference, it is what
the harness implements: `handle_relay_observer_control_event`
(`crates/buzz-acp/src/lib.rs`) dispatches on those two and logs-and-drops
everything else. A third control type added on the daemon side would be
*silently* discarded by the agent, which is the worst available failure shape.
Stated in the contract so nobody adds one.

**Permission answering is not a control frame — it is `/message/{id}/ask`.** See
§3.4 for the full mechanism and the routing precondition; the endpoint publishes
a threaded kind:9 reply carrying `askReplyContent` / `askReplyMentions`
semantics from `desktop/src/features/messages/lib/askCard.ts`, **including the
`broadcast` tag** (a thread-only reply never reaches the channel window and the
card's answered-state derivation breaks without it).

Everything else in `daemon-api.md` (backend deploy passthrough, moderation,
media, DM open/hide, forum, emoji sets) lands in the wave that needs it, and is
absent from `capabilities[]` until then.

### 2.5 Auth and key handling

The repo already has a discipline for this and the daemon follows it exactly.
`buzz-backend-ssh` states it plainly: *nothing secret is ever an argument* — not
to `ssh`, not to a child process. Secrets travel on stdin or on an authenticated
channel, never in `argv` (world-readable in `/proc`), never in an inherited
environment (leaks to every child, appears in crash dumps and process listings).

**Where the nsec lives at rest.** `~/.local/share/buzz/identity/<pubkey8>.ncryptsec`
— NIP-49 scrypt-encrypted, `0600` in a `0700` directory. **[D-8]** This is
deliberately the *same format the desktop's encrypted-backup flow already
produces* (`create_ncryptsec_backup` / `verify_ncryptsec_backup`), so an
operator's existing desktop backup file is a directly importable TUI identity
with no conversion step and no second format to maintain.

**How the daemon gets it. The raw nsec never crosses a process boundary — only
the passphrase does.** Three paths, and no fourth:

1. **Spawn-time.** `buzz-daemon --identity-ncryptsec <path> --passphrase-stdin`.
   The **path is on argv** (it is not a secret) and the **passphrase arrives on
   stdin**, one line, then stdin closes. The daemon does the scrypt decrypt
   itself. This is the auto-spawn path (§2.3) and the default.
2. **`POST /session/identity` over the UDS.** Body is **exactly**
   `{ncryptsec_path, passphrase}` — there is no `{nsec}` form. Used by a second
   client attaching to a daemon whose session was dropped, and by
   `buzz-tui daemon login`.
3. **`--identity-credential <name>`** — reads the passphrase from
   `systemd-creds` (or, with `--passphrase-file <path>`, from a `0600` file whose
   directory is not group- or world-writable). This is the **unattended** path
   and it exists because the VPS install (§6.5) is the flagship one: a
   systemd-launched daemon after a 03:00 reboot has no TTY and no attached
   client, so with only paths 1–2 it would run keyless — decrypting nothing and
   archiving nothing until a human attached a TUI and typed a passphrase. That
   is the always-on observer archive silently failing in exactly the unattended
   case it exists for. Note what this path does *not* weaken: the secret is still
   never in argv and never in an inherited environment.

**An earlier draft offered `{nsec}` on `POST /session/identity` and called the
ncryptsec form "preferred." A preference is not a boundary.** The `{nsec}` form
puts raw key material in the TUI's JS heap, where Bun cannot zeroize it (there is
no `zeroize` for a JS string; the value sits in the GC nursery and in a `fetch`
request-body buffer) and where any HTTP debug logging added later would capture
it. That is the same argument this section already makes against the env-var
path, applied to a path the draft did not notice. It is deleted. If a raw-nsec
import is ever needed for onboarding, it is `buzz-tui identity import` — one
shot, writes an ncryptsec to disk, then uses path 1 — not a daemon endpoint.
§6.4's CI boundary check asserts `{nsec}` is absent from the generated client.

**Keyless is a visible state, not a quiet one.** A daemon running without an
identity reports `archiving: false` on `GET /health`, and every attached TUI
renders it in the status bar as a loss state. A keyless daemon must never look
identical to a healthy one (§1.3 property 3).

**Reading the ncryptsec.** [D-8] The daemon reads **log-n from the ncryptsec
header** (NIP-49 carries it) and never assumes a default — the desktop's backup
uses a repo-chosen `BACKUP_LOG_N = 18` via `create_backup_with_log_n`, not a
NIP-49 default, so a hardcoded assumption fails on real desktop backups. The
decrypt runs **off the async runtime** (`spawn_blocking`, as the desktop does);
scrypt at that cost on a tokio worker would wedge every other socket client for
the duration.

There is **no environment-variable path**. `buzz-cli` accepts `BUZZ_PRIVATE_KEY`
because it is a one-shot process invoked by a harness that controls its
environment; a long-lived daemon that spawns provider subprocesses is a
different threat model. The daemon reads the variable only to *refuse* it, with
a message pointing at `--passphrase-stdin`. No opt-in flag, no dev-only escape
hatch — a special case here is exactly the kind of parallel path that ends up
being the one everyone uses.

**N communities, one passphrase prompt.** §2.2's one-daemon-per-(relay,
identity) means a three-community startup would otherwise be three passphrase
prompts, every time. The TUI prompts **once**, holds the passphrase in a
zeroizing buffer for the duration of the startup fan-out, feeds each spawn's
stdin, and zeroizes. Same identity, same key, no new trust boundary crossed.

**NIP-OA auth tags are identity, and they expire.** Every write path in this
codebase threads one: `BuzzClient::sign_event` **hard-fails** when the auth-tag
count does not match the configured tag, `with_auth_tag` sets `x-auth-tag` on
every HTTP bridge call, and `connect_authenticated(url, &Keys, auth_tag)` carries
it into the NIP-42 kind-22242 event. Without a story here, [D-2]'s send path is
rejected by `sign_event`'s own enforcement. So:

- **Acquisition and storage**: the auth tag lives beside the ncryptsec in
  `~/.local/share/buzz/identity/<pubkey8>.authtag`. It is a *capability*
  credential, not a secret — but it is identity-bound, it expires, and it is
  part of the socket-path preimage (§2.2).
- **`verify_auth_tag` runs at load.** A malformed or already-expired tag is its
  own distinct error, never a generic auth failure.
- **Two signing entry points, mirroring the CLI**: `sign_event` (asserts exactly
  one auth tag, ported verbatim) and `sign_event_unchecked`. The second exists
  because NIP-IA 9035/9036 must *not* carry the ambient tag — a daemon with one
  signing path gets that wrong.
- **Expiry is detected proactively**, from the tag's `created_at<t` /
  `created_at>t` conditions, so `auth_failed{reason: "oa_expired"}` fires
  *before* the relay rejects — §2.6 already promises that UX.

**In memory.** The key material is held in a `zeroize`-wrapping type (already a
`buzz-backend-ssh` dependency); the passphrase buffer likewise. `DELETE
/session/identity` zeroizes and disconnects while keeping the daemon alive.

**Socket authorization, and what it does *not* cover.** `0600` on the socket,
`0700` on its directory, plus a **peer-credential check** on accept —
`SO_PEERCRED` on Linux, `LOCAL_PEERCRED` on macOS — rejecting any connection
whose uid is not the daemon's own. Filesystem permissions are the authorization
model [from `daemon-api.md` §6.9]; peercred is the belt to that suspenders on a
box with a mis-permissioned runtime directory. No TCP listener ships.

**Peercred is a local-only defense. For the remote case the trust boundary is
the SSH session, and saying otherwise is wrong twice over.** An earlier draft
claimed `ssh -L` "keeps 'if you can open the socket you are the user' true across
the network." It does not:

- The forwarded connection is made by the **sshd/ssh process on the daemon
  host**, so peercred reports *that* process's uid. When you SSH in as the same
  user the check passes — but it is passing on the SSH login's uid, not on any
  property of the laptop-side client. It authorizes the SSH session, and the SSH
  session was already the boundary.
- The **laptop end** is the real hole. `ssh -L` creates the local socket with the
  process umask, not `0600` — and a draft example put it in `/tmp`, a
  world-writable directory. Anyone on the laptop who can connect to it gets a
  fully authenticated Buzz session including observer plaintext, and no
  daemon-side peercred check can see them.

So the forward endpoint is specified, not suggested: **`$XDG_RUNTIME_DIR/buzz/fwd/<hash>.sock`
under a `0700` directory, never `/tmp`** (§6.5 carries the exact command), and
**`buzz-tui --socket <path>` refuses to connect** to a socket whose parent
directory is group- or world-writable — the same posture this design already
takes on the SQLite file.

**Redaction, both directions.** The daemon's own log sink passes through a
redactor, and re-applies it to backend-provider stderr on the way in — the
provider already scrubs on the way out, and keeping both layers means a leak
requires two independent failures. The TUI never logs a response body.

The two layers must not be allowed to silently diverge.
`buzz_backend_ssh::protocol::redact` currently matches exactly `["nsec1",
"sprt_tok_"]`. The daemon's list is a **documented superset** adding
`ncryptsec1`, with a test asserting `daemon_prefixes ⊇ provider_prefixes` so an
upstream addition that the daemon has not picked up fails the build rather than
quietly opening a hole. (Preferred long-term: add `ncryptsec1` upstream and
import `protocol::redact` so there is one definition. The superset test is what
holds until then.) Note the upstream scanner's behaviour before "fixing" it: it
scans to the next whitespace or quote and takes `unwrap_or(out.len())` otherwise,
so a secret at end-of-line with no delimiter redacts the rest of the string.
That is correct for a redactor.

**Agent identity minting.** Deploy requires a minted `private_key_nsec` and fails
closed without one. The **daemon** mints it and injects it into the provider
request; the TUI never sees an agent nsec any more than it sees the owner's.

**Observer decryption.** The daemon needs the owner secret key and nothing else —
NIP-44's ECDH is symmetric over the pair and the agent half rides on the event.
There is no key registry, no provisioning, no per-agent secret. Every frame runs
the full guard chain before it can reach any client:

```
0. |event.created_at − now| ≤ 300 s        → drop + count   (freshness / anti-replay)
1. event.verify_id()                       → drop + count
2. event.verify_signature()                → drop + count
3. event.pubkey == tags["agent"]           → drop + count   (sender is who it claims)
4. that pubkey ∈ known agent registry      → queue, then drop + count  ([D-3])
5. content_looks_like_nip44 (132..=87_472) → drop + count
6. nip44::decrypt(owner_secret, …)         → drop + count
7. plaintext ≤ 65_535                      → drop + count
8. dedup on (agent_pubkey, seq, timestamp)
```

**Guard 0 is not optional and was missing from an earlier draft.** Kind 24200 is
ephemeral: the relay never stores it, so there is no relay-side dedup to fall
back on. Anyone who can capture a frame — a relay operator, a MITM on a non-TLS
dev relay, a second client — can replay it into the daemon indefinitely, and the
daemon would archive it as new history. The `(agent_pubkey, seq, timestamp)`
dedup at guard 8 does not save you: a replay carries the same triple and is
deduped only while the daemon still holds that window; after eviction it
re-enters. Both sides of the harness already apply this window — the relay
rejects observer frames outside ±300 s (`crates/buzz-relay/src/handlers/event.rs`)
and the agent applies the identical `OBSERVER_CONTROL_FRESHNESS_SECS = 300`
(`crates/buzz-acp/src/lib.rs`). The daemon is the third party that needs it.

Correspondingly, the archive schema makes `(agent_pubkey, seq, timestamp)` the
frame's **archive identity** with an idempotent upsert, so even a within-window
replay is a no-op rather than a duplicate row.

Guards 1–2 come from the desktop's `decrypt_observer_event`; guards 3–4 come from
`observerRelayStore.ts`'s application-level check; guard 4 buffers before it
drops, per [D-3]. All nine belong in the daemon, and every counter is exposed on
`GET /daemon` so "why is this agent's feed empty" has an answer.

**Ownership gate for agent memory** (Wave 3, stated here because it is a key
question): engrams are encrypted to the *declared owner*, and
`viewerIsOwner = isDeclaredNipOaOwner || iHoldTheSeckeyLocally`. Those two
diverge exactly for a remote-owned agent — the remote-first case. The daemon
must use the **declared-owner** half. Using the local-seckey half would make a
remote-first client unable to read its own agents' memory.

### 2.6 Reconnect

Two links, either can fail independently, and they fail differently:

```
TUI ──HTTP+ndjson──▶ buzz-daemon ──WebSocket/NIP-42──▶ relay
      link A                            link B
```

**Link B (daemon ↔ relay)** is the hard one and its behaviour is inherited whole
from `relay.rs`: startup backoff ladder 1/2/4/8/16 s shared by initial connect
and reconnect; a flat 2 s ±20% DNS retry that is deliberately *not* a backoff
rung (a DNS brownout is not congestion); `STABLE_CONNECTION_SECS = 60` resets the
ladder after a healthy run; 30 s ping / 10 s pong to catch half-open sockets;
`REQ_PACING_INTERVAL = 125 ms` with one REQ per loop tick so a 48-channel
resubscribe does not burst past the relay's ~50-frames/5 s admission; per-channel
`since` watermarks with 5 s skew tolerance replayed on reconnect.

The connection state enum is surfaced verbatim to the TUI, because these states
look identical to "hung" if you collapse them:

```
disconnected | connecting | authenticating | connected | rate_limited
| reconnecting{attempt, next_retry_in_ms} | dns_brownout | auth_failed{reason}
```

**Link A (TUI ↔ daemon)** reconnects on the same exponential shape (1 s → 30 s
cap) and resumes with `GET /event?since=<seq>` against the daemon's 10k ring. If
the cursor has aged out, the daemon sends `stream.reset` **first**, and the TUI
invalidates and re-fetches rather than presenting a silently gapped timeline.

Both links render as **chrome, not toasts** (ux-patterns P26): a two-segment
indicator in the status bar, each segment showing connected / reconnecting
(attempt + countdown) / failed. Auth failure is visually and textually distinct
from network failure, with its remediation inline — a NIP-42 rejection or an
expired NIP-OA auth tag says so and offers `:login`, it does not say
"disconnected".

### 2.7 Offline behaviour

No write queue [non-goal, §1.4]. But no silent loss either:

- A send while link B is down returns `503 relay_unreachable`. The TUI **keeps
  the composed text in the composer**, marks it with a pending state, and binds
  an explicit retry (`<leader>Enter`). The per-channel draft store persists it
  across a TUI restart, so a crash mid-outage does not lose the text.
- A send while `rate_limited` is armed returns `503` with `retry_after_ms`, and
  the composer shows a live countdown rather than a spinner.
- Reads degrade to the cache and the header shows a staleness marker: the channel
  list, rosters, profiles, read-state, and archived observer frames are all
  served from SQLite. What the operator loses while offline is *new* events, and
  the unread divider anchors to an event id (never a row index) so a reconnect
  burst never loses their place.
- Moderation writes (9040–9044) never auto-retry on an ambiguous outcome. They
  execute at the relay *before* dedup, so a blind resend can duplicate the
  mutation. `409 delivery_unknown` renders as "unknown — check the audit log",
  with the audit view one keystroke away. Only a TCP connect error or a
  pre-ingest `429` carrying a `rate-limited:` body is retried internally.

---

## 3. UX specification

### 3.0 The shell

Every screen is the same four-region shell. Only the middle changes.

```
┌ rail ┬ list ─────────┬ main ──────────────────────────┬ aux ───────────┐
│  ●   │ channels /    │  timeline / transcript /       │ thread /       │
│  ○   │ inbox filters │  search results / fleet        │ members /      │
│  ○   │               │                                │ agent detail   │
├──────┴───────────────┴────────────────────────────────┴────────────────┤
│ composer (main-scoped)                                                  │
├─────────────────────────────────────────────────────────────────────────┤
│ status bar: links · pending-leader · counts · context                   │
└─────────────────────────────────────────────────────────────────────────┘
```

Regions collapse by responsive tier (§3.9), never by squeezing. The status bar
and composer are the last things to go, and neither ever does. Within the status
bar, segments drop right-to-left in the priority order §3.9 declares —
**connection and pending-leader never drop**, at any tier.

### 3.1 Main chat view

**LG tier, drawn at 118×31.** `▏` marks the focused pane's left edge.

> **Mock widths are measured, not asserted.** Every mock in this document
> previously carried a declared tier size that it was not drawn at — this one
> said "XL, 160×44" while measuring 118×31; §3.2 said 120×36 and measured 104×22;
> §3.4 measured 104; §3.5 measured 92. No tier boundary in §3.9 had therefore
> ever been validated against real content, which is how the MD floor ended up
> where it did. Labels are now corrected to measured reality, and
> `just tui-check-mocks` (§6.2) parses every fenced block in this file, computes
> its display width with the same grapheme/east-asian-width function the renderer
> uses, and **fails if the measured width does not match the label**. Before Wave
> 1 locks the shell, each screen additionally gets a redraw at its true tier
> ceiling (XL 180, MD 100, MD-narrow 72) — that exercise is expected to move
> boundaries, and moving them on paper is free.

```
┌────┬──────────────────────┬─────────────────────────────────────────────────────────────────┬──────────────────────┐
│ ●B │ ▏#  CHANNELS      12 │  # engineering                                    27 members  ⚙ │  Thread              │
│ ○f │   # general        3 │  topic: relay + desktop · agents welcome                        │  ──────────────────  │
│ ○p │   # engineering   ●9 │ ─────────────────────────────────────────────────────────────── │  troy  14:02         │
│    │   # design           │                                                                 │  can you take the    │
│ ―― │   # random         ★ │   troy                                            14:02         │  40008 diff rows?    │
│ +  │                      │   can you take the 40008 diff rows?                             │                      │
│    │   THREADS          2 │   ♥2  💯1                                          ⤷ 4          │  ⤷ claude-1  14:02   │
│    │   ⤷ 40008 diff rows  │                                                                 │  On it — the aux     │
│    │   ⤷ read-state slots │   claude-1 ⬤                                       14:02        │  fetch is by #e, so  │
│    │                      │   On it. The aux fetch is keyed by `#e` over loaded ids, not    │  a late edit still   │
│    │   DMS              1 │   by the time window, so a late edit for an old visible         │  applies.            │
│    │   ◍ matt          ●2 │   message still applies. Two queries, not one.                  │                      │
│    │   ◌ ops-bot          │                                                                 │  ⤷ troy  14:05       │
│    │                      │   ┌ diff · crates/buzz-daemon/src/session.rs ── +18 −4 ───────┐ │  right. and the      │
│    │   AGENTS           4 │   │  86 │- let since = last_seen;                            │ │  non-conversational   │
│    │   ⬤ claude-1  ⚡ 2   │   │  86 │+ let since = last_seen.saturating_sub(SKEW_SECS);   │ │  kinds don't count   │
│    │   ⬤ claude-2        │   │     │                                    ⏎ expand · y yank │ │  toward unread.      │
│    │   ◐ goose-1   waking │   └─────────────────────────────────────────────────────────────┘ │                    │
│    │   ○ codex-1  offline │                                                                 │  ─── 3 replies ───   │
│    │                      │ ── ● 4 new messages ─────────────────────────────────────────── │                      │
│    │                      │                                                                 │  ▏> reply…           │
│    │                      │   matt                                            14:09         │                      │
│    │                      │   @claude-1 what's the 44200 cadence?                           │                      │
│    │                      │                                                                 │                      │
│    │                      │   ✎ claude-1 is typing…                                         │                      │
├────┴──────────────────────┴─────────────────────────────────────────────────────────────────┴──────────────────────┤
│ > and the `msg:` markers are grow-only per reply id so reading an ancestor never covers a descendant▁              │
├────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ #engineering · thread ⤷40008 │ ⬤relay ⬤daemon │ 12 unread · 2 mentions │ ⚡2 turns │ ^X… │ ? help                  │
└────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Load-bearing details:

- **Author grouping and day dividers** port from `messageGrouping.ts`. Consecutive
  messages by one author within the window collapse to one header.
- **The unread divider (`── ● 4 new messages ──`) is anchored to an event id**,
  never to a row index. It must survive a reconnect burst, a re-render, and a
  tier change. This is the single most-tested piece of chat chrome (§5).
- **Diff messages (kind 40008) get their own row**, not an attachment chip.
  Collapsed to a header plus the changed-hunk summary; `Enter` expands to full
  unified diff; `y` yanks via OSC 52. Terminals were built for this and it is
  the clearest "better here than on the desktop" win in Wave 1.
- **Agent presence glyphs are four-state and semantically exact**: `⬤` present
  (relay presence), `◐` waking, `○` offline, `◌` **unknown** — no presence beat
  seen since this daemon started and no 40902 snapshot yet (§2.4). `unknown` is
  never collapsed into `offline`. The desktop's remote-agent liveness
  asymmetry applies verbatim — a provider-backed agent's `deployed` status never
  clears (the provider protocol has no undeploy), so `backend_agent_id` being set
  says nothing about liveness. **Liveness for remote agents comes only from relay
  presence.** The TUI renders presence, and shows deploy status only in the agent
  detail pane, explicitly labelled "last deploy succeeded" rather than "running".
- **Reaction strips** render as `♥2 💯1` under the message; `⤷ 4` is the thread
  reply count from kind 39005.
- **Non-conversational kinds** (40099 system, 43001–43006 job, **48100** huddle
  started) render as their own dimmed rows and are excluded from the unread pill,
  per `isConversationalUnreadKind`. The list is `TIMELINE_KINDS` verbatim
  (`channel_window.rs`): **48100 only**. 48101–48103 are neither fetched nor
  rendered in Wave 1 — an earlier draft rendered them while fetching only 48100,
  which is unimplementable as written and made a §5.2 test case assert on a path
  that cannot occur. They arrive with the read-only huddle surface in Wave 4.

### 3.2 Thread view

Threads have three modes, carried over from the desktop's
`threadViewModePreference`: **docked** (the aux pane above), **focused**
(full-width, list pane retained), and **detached** (its own pane that survives
channel navigation — the desktop's "independent" mode, and the one that matters
most on a VPS where you park an agent thread and keep working).

Focused mode, drawn at 104×22:

```
┌────┬──────────────────────┬──────────────────────────────────────────────────────────────────────────┐
│ ●B │   # engineering      │  ⤷ Thread · #engineering                              4 replies · ⇧⏎ ↩   │
│ ○f │ ▏⤷ 40008 diff rows   │  ─────────────────────────────────────────────────────────────────────── │
│ ○p │   ⤷ read-state slots │  troy                                                          14:02    │
│    │                      │  can you take the 40008 diff rows?                                       │
│    │  ── in thread ────── │  ♥2  💯1                                                                 │
│    │   troy               │                                                                          │
│    │   claude-1           │    └ claude-1 ⬤                                                14:02    │
│    │   matt               │      On it. The aux fetch is keyed by `#e` over loaded ids.              │
│    │                      │                                                                          │
│    │  following  ✓        │      └ troy                                                    14:05    │
│    │  ^X f  unfollow      │        right, and non-conversational kinds don't count toward unread.    │
│    │                      │                                                                          │
│    │                      │    └ matt                                                      14:31    │
│    │                      │      ┌ orphan — parent not loaded ──────────────────────────────┐        │
│    │                      │      │  ^X b  backfill ancestors                                │        │
│    │                      │      └──────────────────────────────────────────────────────────┘        │
├────┴──────────────────────┴─────────────────────────────────────────────────────────────────────────┤
│ > ▁                                                                                                  │
├──────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ ⤷ thread (focused) · esc back · ^X t detach │ ⬤relay ⬤daemon │ following │ ? help                    │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Reply nesting uses `threadTreeLayout.ts`'s shape with `└` guides, capped at 3
visual indent levels (deeper replies stay at level 3 with a `·3` depth marker) so
a deep thread does not become a 1-column column. Orphan replies whose parent is
not loaded render an explicit backfill affordance rather than silently hiding —
the desktop's `useLoadMissingAncestors` behaviour, made visible.

### 3.3 Mention picker

Triggered by `@` in the composer. Mechanics are ux-patterns P11 verbatim:
trigger detection is a pure function of (text, cursor offset) with the three
rules (nearest `@` backwards; preceded by start-of-input or whitespace so
`foo@bar` does not trigger; no whitespace between `@` and cursor); all offsets
are **display widths** via grapheme segmentation, never byte or JS-string
indices; `input: "keyboard"` is forced on every re-filter so a stationary mouse
cannot hijack the selection; the previous list is held while an async fetch is in
flight so the popup does not flicker.

```
│   matt                                                       14:09      │
│   @claude-1 what's the 44200 cadence?                                   │
│                                                                         │
│    ┌──────────────────────────────────────────────────────────────┐     │
│    │  PEOPLE                                                      │     │
│    │ ▸1 @matt             Matt · online              recent ▓▓▓▓  │     │
│    │  2 @troy             Troy · online              recent ▓▓▓   │     │
│    │  AGENTS                                                      │     │
│    │  3 @claude-1         claude-agent-acp · ⬤ · ⚡2 turns  ▓▓▓▓▓ │     │
│    │  4 @claude-2         claude-agent-acp · ⬤                    │     │
│    │  5 @goose-1          goose · ◐ waking                        │     │
│    │  6 @codex-1          codex · ○ offline · last seen 3d        │     │
│    │──────────────────────────────────────────────────────────────│     │
│    │  ⏎/⇥ insert  alt+N pick  ^n/^p move  ^s scope  esc close     │     │
│    └──────────────────────────────────────────────────────────────┘     │
├─────────────────────────────────────────────────────────────────────────┤
│ > hey @ma▁                                                              │
```

Buzz-specific rules on top of the pattern:

- **Humans and agents are one list, sectioned, not two pickers** [LOCKED: "@-
  mentions with autocomplete (humans and agents)"]. Sections are visual only; a
  single selection cursor runs through all of them.
- **Ranking**: exact-prefix match doubles the score, then multiply by
  `(1 + frecency)` where frecency is `frequency / (1 + ageInDays)` over *people
  and channels* (ux-patterns P12), persisted as append-mostly JSONL in the TUI's
  state dir. Local entities (roster members) always outrank directory-search
  results — the merge order is `[...ranked_roster, ...directory]`.
- **Agent liveness is shown in the picker**, because mentioning an offline agent
  is a silent no-op otherwise. Offline agents are not hidden (you may
  deliberately queue for one) but are dimmed and carry a last-seen.
- **`⇥` completes. It is an alias of `⏎`, not a second verb.** Tab means
  *complete* in every shell, every readline app, and every editor these two
  operators touch; rebinding it to a narrowing operation inside a completion
  popup guarantees permanent mis-fires, and it collided with `pane_next` in base
  mode, making Tab mean three things depending on invisible state. **Scoping
  moves to `ctrl+s`** and is labelled in the popup footer, where it is
  discoverable without stealing the one key with a universal prior.
- **Selection is deterministic, because frecency ranking is not.** With the
  roster above, `frecency = frequency/(1+ageInDays)` decays continuously, so the
  row order for `@c` changes between sessions — you can never build muscle
  memory, and every mention costs a visual confirmation. On a phone-mirrored pane
  with render latency, that confirmation *is* the expensive part, not the
  keypresses. Prefix-colliding agent names (`claude-1`, `claude-2`, `claude-3` —
  the orchestrator's normal case) make it worst exactly where it is used most.
  So: **each row carries a stable numeric index and `alt+1`…`alt+9` picks it
  directly.** `@c` + `alt+2` is four keystrokes regardless of how frecency
  ordered the list.
- **`@@` is an agents-only trigger**, mirroring `#` for channels, so the
  orchestrator's dominant case skips the PEOPLE section entirely.
- **Enter with zero candidates sends nothing and inserts nothing.** It is a
  no-op that keeps the popup open with a `no matches` footer; `esc` dismisses to
  literal text. Leaving this undefined risks the worst outcome — a half-composed
  message sent by a reflexive Enter.
- **`@channel` is not in Wave 1.** It appeared in an earlier draft of this mock
  as `@channel — notify 27 members`; it has no wire representation anywhere in
  the protocol (see [D-2]), and client-side expansion breaks against
  `MENTION_CAP = 50` as a community grows. Tracked as Q8.
- **Insertion** is delete-range-then-insert, then extmark, then part (P11g):
  trailing space added only if the following char is not already a space;
  duplicate mentions update the existing part's offsets rather than appending a
  second one.
- **Non-member guard**: mentioning someone not in the channel opens a modal
  whose options are arrow-selected and `⏎`-committed (`add to channel` /
  `mention anyway` / `esc` cancels), carrying over the desktop's
  `NonMemberMentionDialog`. Adding a member to a channel is a membership write —
  it does not get a bare single letter (§3.7 rule 2).
- The composer's **parts array maps directly onto the event's tags** — `p` for
  user/agent mentions, `h` for the channel, `e` for message refs. This is a
  strictly better fit than opencode's, where parts must be flattened for an LLM.

Also on the same primitive: `#` completes channels, `:` completes emoji
shortcodes (rendering the literal `:shortcode:` for custom emoji rather than
faking the image), `/` opens slash commands.

### 3.4 Agent fleet view

**The flagship, and the default landing screen for `<leader>a`.** The
single-agent transcript (§3.4.1) is what you open *from* here.

The orchestrator's question is not "what is this agent doing" — it is **"which of
my agents needs me."** A design that ships only the transcript gives a beautiful
microscope aimed at one agent while the actual daily job is knowing which of
eight is blocked on a permission prompt, which is looping, and which finished
twenty minutes ago. Deferring that to the Wave-2 home inbox would mean the daily
loop is "TUI for the agent I'm already watching, desktop for figuring out *which*
agent to watch" — which keeps the desktop open, which keeps it primary, which
means the five-day exit criterion gets negotiated rather than met.

It is also strictly *less* work than the transcript: a reduction over data the
daemon already folds, served by `GET /agent/fleet`, and it degrades to XS
beautifully because it is a table of short numbers.

Drawn at 104×13:

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────────┐
│  FLEET · 6 agents · 1 blocked · 2 working                            sort: blocked ▸ working ▸ idle  │
│ ─────────────────────────────────────────────────────────────────────────────────────────────────── │
│  agent        state      turn   elapsed    in/out      $      ctx    tok/min   ⚠                     │
│ ▸claude-1  ⚠ blocked     4a91   00:04:12   12k/1.9k   0.041   29%       310    write session.rs      │
│  goose-1   ⚡ working     7c02   00:00:48    3k/0.4k   0.008    —        620                         │
│  claude-2  ⚡ working     91ab   00:12:30   88k/9.1k   0.412   64%     1,240    ↻ same tool ×7       │
│  matt-bot  ✓ idle          —    12m ago    —          —        —          —                          │
│  codex-1   ○ offline       —    3d ago     —          —        —          —                          │
│  ops-bot   ◌ unknown       —    —          —          —        —          —                          │
│ ─────────────────────────────────────────────────────────────────────────────────────────────────── │
│  ⏎ open transcript   a answer blocked   ^r refresh                                    6 agents       │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Rules:

- **Sorted blocked-first, always.** Blocked ▸ working ▸ idle ▸ offline ▸ unknown;
  within a class, longest-waiting first. Sort order is not a preference.
- **`↻ same tool ×N` is the loop detector** — a repeated-identical-tool-call
  counter over frames the daemon already folds. It is roughly twenty lines and it
  is the single highest-value cost signal in the product: totals tell you what
  you spent, the loop counter tells you what you are *about* to spend.
- **`tok/min` is burn rate**, not a total. "Is this agent stuck burning money" is
  the actual cost question and no cumulative figure answers it.
- **`—` means not reported**, never `0` (the §3.4.1 null rule applies here too).

#### 3.4.1 Agent activity view (single agent)

Reached by `Enter` on a fleet row, or by `Enter` on an agent in the rail. Drawn
at 104×33.

```
┌────┬──────────────────────┬──────────────────────────────────────────────┬──────────────────────────┐
│ ●B │ ▏AGENTS            4 │  ⬤ claude-1  ·  claude-agent-acp             │  USAGE (NIP-AM 44200)    │
│ ○f │  ⬤ claude-1   ⚡2    │  session 8f3c…  ·  #engineering              │  ──────────────────────  │
│ ○p │  ⬤ claude-2         │ ──────────────────────────────────────────── │  this turn                │
│    │  ◐ goose-1   waking │  ⚡ turn 4a91  started 14:02:11  · 00:04:12 ↻ │   in    12,480            │
│    │  ○ codex-1  offline │                                              │   out    1,932            │
│    │                      │  ● 14:02:11  turn started                    │   cache r 9,120 w 340     │
│    │  ── filter ^X f ──── │                                              │   cost   $0.0412          │
│    │  ▸ all               │  ▸ 14:02:12  thought                         │                           │
│    │    tools only        │    Need to check how aux events are fetched  │  session cumulative       │
│    │    messages          │    before touching the window query.         │   turns  4                │
│    │    errors            │                                              │   in    58,204            │
│    │    raw ACP           │  ⊙ 14:02:14  read   crates/…/session.rs      │   out    7,880            │
│    │                      │    lines 1–120                    ⏎ expand   │   opus-5    $0.1902       │
│    │  ── channels ─────── │                                              │   sonnet-5  $0.0238       │
│    │  #engineering    ⚡  │  ⚑ 14:02:19  shell  cargo check -p buzz-…    │   burn  310 tok/min       │
│    │  #general            │    exit 0 · 3.4s                  ⏎ expand   │  stop_reason  end_turn ×3 │
│    │                      │                                              │               max_tokens×1│
│    │  ── controls ─────── │  ✎ 14:02:31  edit   session.rs  +18 −4       │  ──────────────────────  │
│    │  ^X c  cancel turn   │    ┌────────────────────────────────────┐    │  MODEL                    │
│    │  ^X M  switch model  │    │ 86│- let since = last_seen;        │    │  claude-opus-5            │
│    │  ^X l  harness log   │    │ 86│+ let since = last_seen.satu…   │    │  ctx 58,204 / —           │
│    │                      │    └────────────────────────────────────┘    │  (no reported window)     │
│    │                      │                                              │                           │
│    │                      │  ⚠ 14:02:44  ask · awaiting owner            │  ──────────────────────  │
│    │                      │                                              │                           │
│    │                      │    write crates/buzz-daemon/src/session.rs   │  ⚠ 1 awaiting response    │
│    │                      │    ▸ allow once   always   deny      ⏎ send  │    (answers post to      │
│    │                      │    ↳ replies into #engineering               │     #engineering)        │
│    │                      │  ⟳ 14:02:51  liveness                 (10s)  │                           │
├────┴──────────────────────┴──────────────────────────────────────────────┴──────────────────────────┤
│ agent claude-1 · turn 4a91 · live │ ⬤relay ⬤daemon │ 3000 frames buffered · 0 dropped │ ? help      │
└─────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

This view is a rendering of the **decrypted observer stream folded into a
transcript**, and the folding happens in the daemon (`GET
/agent/{pk}/transcript`), not in the TUI — because it is a stateful machine with
correlations that must not be implemented twice:

- `acp_write` `session/new` → a standalone **System prompt** metadata card with
  its sections parsed (`[Base]`, `[System]`, `[Agent Memory — core]`,
  `[Channel Canvas]`), rendered with `turnId: null`.
- `acp_write` `session/prompt` → a user message plus a **Prompt context** card;
  the prompt text is parsed for the triggering event id so the row links back to
  the originating channel message.
- `_goose/unstable/session/steer` → same as prompt, keyed `steer:*`, and it
  **suppresses the `user_message_chunk` echo** Goose sends back.
- `session/request_permission` → a permission item indexed by JSON-RPC `id`; the
  bare `{id, result:{outcome:{…}}}` that follows resolves *that* item. Correlating
  by JSON-RPC id is the part a naive implementation gets wrong. **Rendering a
  permission item is not the same as being able to answer it** — see below.
- `acp_read` `session/update` dispatches on `update.sessionUpdate`:
  `agent_message_chunk` coalesced by `messageId`, `agent_thought_chunk`,
  `tool_call` → `tool_call_update` updating the *same* item id,
  `plan` with update targeting, `usage_update` → the Usage row, and
  **anything unrecognized is dropped, never guessed**.

Render classes and tones come from `agentSessionToolClassifier.ts` and map onto
glyphs: `⊙` read · `✎` write/edit · `⚑` shell · `▸` thought · `☰` plan ·
`⚠` permission · `✖` error · `●` lifecycle · `⋯` suppressed · `#` raw rail.

**Answering a permission is a channel message, not a control frame — and most of
the time there is nothing to answer.** An earlier draft rendered an inline
`[y] allow once [A] always [n] deny` row and routed the answer through
`POST /agent/{pk}/control`. Both halves were wrong:

- **The control channel has no permission type.**
  `handle_relay_observer_control_event` dispatches on `cancel_turn` and
  `switch_model` and logs-and-drops anything else, so such a frame would vanish
  silently (§2.4).
- **The real answer path is an ask card.** `PermissionRouting::AskOwner`
  (`crates/buzz-acp/src/acp.rs`) parks the request and `ElicitationAsk::publish`
  posts an ordinary **kind:9 message into the channel** carrying an
  `["ask", <json>]` tag. The owner answers by publishing a **threaded reply**
  whose parent is `ElicitationState::question_event_id()`. This is exactly what
  the desktop's `AskMessageCard.tsx` does — a NIP-CW broadcast reply, not a
  control frame.
- **The precondition is usually absent.** The harness default is
  `permission_mode = bypass-permissions` (`crates/buzz-acp/src/config.rs`), i.e.
  `PermissionRouting::Auto` — permissions are auto-approved and never reach the
  owner at all unless the agent was deployed with `--permission-mode askOwner`.

So the transcript renders the permission item **inline where it happened**, but
the answer travels through `POST /message/{id}/ask` (§2.4) as a threaded kind:9
reply with `askReplyContent` / `askReplyMentions` and the `broadcast` tag. The UI
states this: the row shows `↳ replies into #<channel>` so the operator is never
surprised that answering posted a message. Under `Auto` routing the row renders
as **informational and already-answered** (`auto-approved`), with no affordance —
an actionable-looking control that cannot act is worse than no control.

**Ask-card projection is a daemon deliverable**, not a TUI concern, and it is
what feeds the `⚠ 1 awaiting response` counter (which otherwise has no data
source). The daemon parses `["ask", json]` off kind:9/40002 with the validation
from `askCard.ts` **verbatim** — reject `v ≠ 1`, cap options at
`ASK_MAX_OPTIONS = 20`, reject non-array/malformed option shapes (that file's own
comment is explicit that the producer is not the trust boundary) — exposes it on
the hydrated message, emits `agent.ask.open` / `agent.ask.answered` stream
events, and derives the awaiting count from open cards. T0 mirrors
`askCard.test.mjs`: malformed json → null, `v:2` → null, `options: "nope"` →
null.

Liveness and clock skew, ported exactly: per-agent offset is the running
**minimum** of `now - parse(event.timestamp)`; the turn badge anchors to
`startedAt + offset` derived *at read time*, so a later tighter offset
retroactively corrects every live turn. Turns prune after 2.5×10 s of silence,
except that pruning pauses for up to 3 minutes when **all** of an agent's turns
go quiet at once — that simultaneity is the frame-stream-down signature, not
completion. Terminal tombstones stop a late liveness frame resurrecting a
finished turn. Cap 32 concurrent turns per agent, matching `BUZZ_ACP_AGENTS`.

**The right-hand usage pane is net-new capability, not parity.** Kind 44200 is
fully specified, archived by the desktop, and rendered by nothing. Rules that
must hold: `null` token fields mean *not reported*, not zero — render `—`, never
`0`; `totalTokens` is provider-reported and is never derived by summing;
unrecognized `stopReason` values collapse to `unknown`; `turnSeq` is strictly
increasing per session and a gap is displayed as a gap. Cost is only shown when
`costUsd` is present.

Three corrections where an earlier draft's own mock violated those rules:

- **The context-window denominator is provider-reported or absent.** The draft
  rendered `ctx 58,204 / 200,000` with a 29% bar, but **44200 does not carry a
  context-window size** — that 200,000 was a client-side model table, which is
  precisely the thing that rots (opus-5 ships 200k *and* 1M variants). If no
  provider-reported window is present, render `ctx 58,204 / —` and **no bar**.
  Deriving a denominator is the same sin as deriving `totalTokens`.
- **Cumulative cost never blends models.** `[M] switch model` is a first-class
  control in this very pane, so one `session cumulative · $0.2140` across a
  session that switched from opus to sonnet is a meaningless number. Cost breaks
  down **per model**; when more than one model appears in a session there is no
  single-figure total.
- **Totals are not the cost question.** The pane adds **burn rate** (`tok/min`,
  `$/hr`) and the **repeated-identical-tool-call counter** that feeds §3.4's
  `↻ same tool ×N`. "Is this agent stuck in a loop burning money" is what an
  operator actually needs, and no total answers it.

**A one-line usage strip survives to every tier, including XS.** The pane lives
in `aux`, which is an overlay at MD and gone at SM/XS (§3.9) — so as drafted, the
flagship differentiator did not exist on the phone or at 72 columns, while §5.3
still demanded a `usage-pane × xs` snapshot of undefined behaviour. Token burn is
a *number*, not a panel, and it is the last thing that should go. So the
**header** of any agent-scoped screen carries:

```
opus-5 · 58k/— · $0.21 · ⚡2 · ↻7
```

at every tier down to XS. The aux pane is the expansion of that strip, not its
only home.

**The frame counter in the status bar is not decoration.** `3000 frames buffered
· 0 dropped` is how "loss is always visible" manifests here; a non-zero drop
count is rendered in the error token and links to `GET /daemon` counters.

### 3.5 Search

`/` (in-channel find) and `<leader>/` or `ctrl+p`-then-`>` (global search).

```
┌─────────────────────────────────────────────────────────────────────────────────────────┐
│  Search   from:matt in:#engineering after:2026-07-01  read-state slots                   │
│  ───────────────────────────────────────────────────────────────────────  18 / 18  ▓▓▓  │
│                                                                                          │
│ ▸ #engineering · matt                                                    2026-07-28 09:14│
│   32 KB plaintext per slot, up to 8 slots — the **read-state slots** cap is what forces  │
│   the horizon prune.                                                                     │
│                                                                                          │
│   #engineering · troy                                                    2026-07-24 16:02│
│   we should test the slot split at exactly 8, the 9th write is the interesting one       │
│                                                                                          │
│   #general · matt                                                        2026-07-19 11:40│
│   read-state is the highest-risk port. contexts map, max-merge, graph-derived parent     │
│                                                                                          │
│  ─────────────────────────────────────────────────────────────────────────────────────── │
│  ⏎ open in channel   ⇥ open in thread   n/N next/prev   ^y yank link   esc close         │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Slack operators** parse identically to the desktop's
  `parseSearchOperators.ts`: `from:`, `in:`, `after:YYYY-MM-DD`,
  `before:YYYY-MM-DD`. Operators must start at a **token boundary** — deliberately
  not `\b`, so `built-in:react` and `https://x.com/in:foo` are not misparsed.
  `after:` is local start-of-day inclusive; `before:` is one second before local
  start-of-day, because NIP-01 `until` is inclusive and Slack excludes the named
  day. An invalid operator value stays in the FTS text rather than erroring.
- **`kinds` is always set.** Default `[9, 40002, 45001, 45003]`. This is not a
  search-specific rule — it is the global daemon invariant of §2.4 (**no filter
  leaves the daemon without an explicit `kinds`**, because a kindless filter can
  match a `P_GATED_KIND` and is refused). Search is one instance; `/message/{id}`
  lookups are another. Enforced in the daemon so the TUI structurally cannot get
  it wrong.
- **Match count is visible** in the search bar *and* the status bar (`18 / 18`),
  and `n`/`N` navigate. Three-level match hierarchy: gutter marker on the row,
  highlight on the line, stronger emphasis on the active match.
- **Search-as-you-type, debounced 150 ms.** The debounce protects the relay's FTS,
  not just the render loop.
- **`author` may be a display name**; ambiguity returns `409 ambiguous_author`
  with candidates, rendered as a disambiguation list — never a silent mix of
  authors.

### 3.6 Command palette

`ctrl+p`. It is a **projection of the keymap** (ux-patterns P21), not a list.

```
┌──────────────────────────────────────────────────────────────────────────────┐
│  >  mark                                                                     │
│  ──────────────────────────────────────────────────────────────────────────  │
│   CHANNEL                                                                    │
│  ▸ Mark channel as read                                            ^X m      │
│    Mark all channels as read                                       —         │
│    Mark message unread                                     Wave 2            │
│   AGENT                                                                      │
│    Mark agent turn cancelled                                       ^X c      │
│  ──────────────────────────────────────────────────────────────────────────  │
│   ⏎ run   ^n/^p move   esc close                          4 of 87 commands   │
└──────────────────────────────────────────────────────────────────────────────┘
```

Three properties that fall out of the projection and are each a review gate:

1. **Visibility is `reachable`** — only commands live in the current mode/layer
   stack appear. Thread commands do not appear while focus is in the channel list.
2. **Each row shows its current binding**, formatted by the same function the
   help overlay uses, with `<leader>` rendered as the user's actual leader. The
   palette therefore teaches the keybindings.
3. **Empty query shows a "Suggested" section**, contextually predicated —
   "Mark all read" only when unread > 0, "Cancel turn" only when a turn is live.

Commands with **no binding** (`"none"`) are palette- and slash-reachable, and
render their binding column as `—`. That is the right default for the long tail —
`invite`, `set-topic`, `export-snapshot`, `doctor` — and for anything
destructive (`mark_all_read`, §3.7). It is what keeps the keybinding table from
bloating as waves land.

**Unshipped commands render greyed with their wave tag** rather than vanishing.
`reachable`-gating hides what is *contextually* inapplicable; a Wave-2 feature is
not inapplicable, it is not built yet, and greying it teaches the roadmap where
hiding teaches absence and binding it teaches breakage.

### 3.7 Keybinding grammar

One declarative table, `{default, description}` per entry (ux-patterns P1). The
`description` is load-bearing: palette, which-key, and help are all generated
from it. Leader defaults to `ctrl+x`, with a pending-sequence timeout, `escape`
clears, `backspace` pops one token, and **pending state is shown in the status
bar** — an invisible modal state is the number-one way a keybinding grammar feels
broken.

**Two rules the table itself must satisfy:**

1. **Every key the UI advertises is in this table, with a mode.** No exceptions.
   Earlier drafts put bare bracket letters directly in the mocks — `[c] cancel
   turn`, `[M] switch model`, `[L] harness log`, filters `[a][t][m][e][r]`,
   `[a] backfill ancestors`, `[a] add to channel`, `[m] mention anyway`,
   `[D] open`, `[y] yank` — none of which appeared here. That is a second,
   undeclared keybinding grammar, and it directly violates §1.3 property 4. It
   also collided: `y` meant both `message_yank` and "allow this permission";
   `n` meant both "deny" and next-search-match; `[a]` meant three different
   things in three panes. Pressing `y` to yank a transcript line the instant a
   permission arrived would have approved a write to disk.
2. **No destructive or irreversible command gets a bare single-letter binding.**
   Bracket affordances render as *labels for table entries*, never as their own
   grammar, and anything irreversible requires an explicitly focused item with
   the focus state rendered.

**Leader is configurable at first run, with a conflict check.** `ctrl+b` is
correctly avoided as tmux's default prefix — but operators overwhelmingly rebind
that prefix to `ctrl+a` or `ctrl+x`, and this design uses both (`ctrl+a` is
`input.beginning_of_line`; `ctrl+x` was a fixed leader). If the operator's prefix
is `ctrl+x`, the entire leader grammar is unreachable and the app appears frozen
on every chord. So first run reads the live tmux config, and a collision makes
the leader prompt pick something else.

**`buzz-tui doctor`** ships in Wave 1 and reports the terminal/tmux settings this
design depends on, printing the exact `set -sg` lines to paste: prefix collisions,
`escape-time` (default 500 ms makes `ESC`+`j` indistinguishable from `alt+j`,
which is why the `alt+arrow` bindings below are secondary and not sole),
`extended-keys` (without it, and without CSI-u / `modifyOtherKeys` in the outer
terminal, **Shift-Enter is byte-identical to Enter** and multi-line composition
silently sends instead — hence `ctrl+j` is the *primary* documented newline key),
`focus-events` (§3.11), `allow-passthrough` (OSC 9), and `set-clipboard`
(OSC 52, §4.6). Claude Code learned this the hard way and it is why
`/terminal-setup` exists.

**The `input.*` family is copied verbatim from opencode** (MIT, code reuse
permitted): `ctrl+a/e`, `alt+f/b`, `ctrl+w`, `ctrl+k/u`, and the rest. These
encode emacs/readline/macOS conventions already in the operators' fingers; there
is nothing to gain from re-deriving them and everything to lose.

Wave-1 table (~55 entries; the shape, not the whole file — and every advertised
key is here, including ones a mock renders as a bracket label):

| Command | Default | Mode |
|---|---|---|
| `app_exit` | `<leader>q` | any |
| `cancel_or_exit` | `ctrl+c` | any |
| `exit_on_empty` | `ctrl+d` | base, composer (empty only) |
| `command_list` | `ctrl+p` | base, composer (empty input) |
| `help` | `?` | base |
| `leader` | `ctrl+x` (configurable, §above) | — |
| `community_switch` | `<leader>o` | any |
| `channel_list` | `<leader>k` | any |
| `channel_next` / `channel_prev` | `<leader>j` / `<leader>K`, `alt+down`/`alt+up` | base |
| `channel_next_unread` | `<leader>n`, `alt+shift+down` | base |
| `pane_next` / `pane_prev` | `tab` / `shift+tab` | base |
| `pane_resize` | `<leader>` + arrows | base |
| `message_focus_enter` | `escape` (from composer) | composer |
| `message_next` / `message_prev` | `j` / `k` | message-select |
| `message_reply` | `r` | message-select |
| `thread_open` / `thread_close` | `enter` / `escape` | message-select |
| `thread_detach` | `<leader>t` | thread |
| `thread_backfill_ancestors` | `<leader>b` | thread (orphan focused) |
| `message_copy_link` | `<leader>y` | message-select |
| `message_yank` | `y` | message-select |
| `mark_read` | `<leader>m` | base |
| `mark_all_read` | `none` (palette/slash only) | base |
| `jump_first_unread` | `<leader>u` | base |
| `jump_next_mention` | `<leader>@` | base |
| `find_in_channel` | `/` | base |
| `search_global` | `<leader>/` | any |
| `history_prev` / `history_next` | `ctrl+up` / `ctrl+down` | composer |
| `agent_fleet` | `<leader>a` | any |
| `agent_open_transcript` | `enter` | fleet |
| `agent_cancel_turn` | `<leader>c` | agent |
| `agent_switch_model` | `<leader>M` | agent |
| `agent_harness_log` | `<leader>l` | agent |
| `agent_filter_cycle` | `<leader>f` | agent |
| `agent_ask_focus` | `a` | agent, fleet |
| `agent_ask_answer` | `enter` | agent (ask focused) |
| `agent_ask_dismiss` | `ctrl+d` | agent (ask focused) |
| `mention_pick_index` | `alt+1`…`alt+9` | autocomplete |
| `mention_scope` | `ctrl+s` | autocomplete |
| `mention_accept` | `enter,tab` | autocomplete |
| `composer_send` | `enter` | composer |
| `composer_newline` | `ctrl+j,shift+return,ctrl+return,alt+return` | composer |
| `composer_retry_send` | `<leader>enter` | composer (pending) |
| `composer_external_editor` | `<leader>E` | composer |
| `composer_stash` / `_pop` | `none` | palette-only |
| `open_in_desktop` | `<leader>D` | any |
| `theme_cycle` | `none` | palette-only |
| `doctor` | `none` | palette-only |

Four decisions in that table that are not obvious:

- **`mark_all_read` has no binding.** It was `<leader>M` while
  `agent_switch_model` was *also* `<leader>M` — a literal collision — and it sat
  one shift-key away from `mark_read`'s `<leader>m`. Under NIP-RS max-wins merge
  the unread frontier only moves forward: there is no global unmark, so a
  shift-typo permanently destroys unread state across every channel **and
  propagates to the operator's other devices**. It is the textbook case for the
  `"none"` default this design already establishes for the long tail, and it
  additionally requires a **confirm modal naming the channel count**.
  `agent_switch_model` keeps `<leader>M` uncontested.
- **`ctrl+c` cancels before it exits.** Three unconfirmed exits
  (`ctrl+c,ctrl+d,<leader>q`) in a client that lives next to Claude Code all day —
  where the first `ctrl+c` cancels and the second exits — meant `ctrl+c`
  mid-compose quit the app. `cancel_or_exit` adopts that contract exactly: first
  press clears the composer / pops a pending leader / closes the top popup;
  a second press within 2 s exits, with the hint in the status bar.
  `exit_on_empty` (`ctrl+d`) exits **only** on an empty composer in base mode —
  otherwise `ctrl+d` is `input.delete-char-forward` from the verbatim `input.*`
  family, and the draft never said which won. `<leader>q` remains the deliberate
  quit.
- **`alt+arrow` bindings are secondary, never sole.** Alt is transmitted as an
  ESC prefix and tmux's default `escape-time 500` makes `ESC`+`j`
  indistinguishable from `alt+j`; many terminals also bind `alt+shift+arrow`
  themselves. Each has a leader-chord peer that always works.
- **Wave-2 commands are not in the Wave-1 table.** `message_edit` (`e`),
  `message_delete` (`d d`), `message_react` (`<leader>e`), and mark-unread (`u`)
  were bound here while §4.1.3 puts all four in Wave 2 — which ships either dead
  keys (teaching the operator the app is broken) or scope leak. They are removed.
  The palette renders unshipped commands **greyed with a `Wave 2` tag** rather
  than hiding them, which teaches the roadmap instead of teaching absence.

**Launch mode is `composer`.** It is a chat app; the cursor belongs in the input.
This matters because `/` is `find_in_channel` in base and slash-commands in
composer, and `?` is help in base but a literal `?` in composer — leaving launch
focus unstated makes both ambiguous. `command_list` moves off mode `any` to
`base` + empty-composer, freeing `ctrl+p`'s readline meaning inside a
non-empty composer; **prompt history is `ctrl+up`/`ctrl+down`**, not `alt+arrow`
(taken) and not `ctrl+p` (taken).

Binding *values* are a small union, not a string: `false` / `"none"` unbinds,
a comma string or array multi-binds, `{key, preventDefault:false}` lets a binding
run *and* fall through (paste), and an unknown key in user config is a **hard
error naming the offender**, never a silent ignore.

### 3.8 Focus model

Focus is a **mode stack**, never booleans (ux-patterns P6). Pushes return a
disposer that pops **by identity**, so out-of-order cleanup cannot corrupt the
stack — the exact bug `isModalOpen` flags produce.

Modes: `base` · `composer` · `message-select` · `thread` · `autocomplete` ·
`palette` · `modal` · `search` · `agent`.

The single rule that makes a chat TUI feel correct: **typing in the composer must
not fire global single-letter keys.** That is mode-scoping plus the
managed-textarea layer, which installs the `input.*` bindings only when a real
multi-line textarea has focus — and notably *not* for single-line inputs inside
dialogs, which get different editing bindings (a channel-search box should not
have `input.newline`).

Dialogs are a stack with **focus restoration**: save the focused renderable on
first push, and on close verify it still exists in the live tree before
refocusing (a naive `saved.focus()` crashes when the node was destroyed while the
dialog was open). `escape` closes only the top of the stack, and only when no
text is selected — a user dragging to copy does not lose the dialog.

Pane focus cycles with `tab`/`shift+tab` in `base`. **Every mouse affordance has
a keyboard peer** (ux-patterns P9): pane resize is `<leader>`+arrows as well as
drag; split ratios are stored in **basis points** (integers 0–10000) so resize is
idempotent and snapshot-stable; drag is a strict Down→Move→Up machine whose latch
clears on any keyboard event.

**Mouse capture is OFF by default; `:set mouse on` opts in.** Enabling mouse
reporting inside a tmux pane takes away native text selection for copy and native
scroll-wheel scrollback — the two things a terminal-native user reaches for
reflexively, and both of them things these two operators do all day. Since every
mouse affordance is *mandated* to have a keyboard peer, defaulting to off costs
nothing and defaulting to on costs the reflexes. (It also keeps §5.5's
`capture-pane -p -S -` scrollback assertions meaningful for the default
configuration.) This raises the stakes on Q1 — native scrollback plus native
selection is the single biggest "feels like a real terminal app" lever available,
and it is worth the timeboxed spike before the shell locks.

### 3.9 Responsive tiers

Explicit tiers with a distinct tiny fallback, not a flex solver squeezing panes
into illegibility. **The XS and SM tiers are the phone-via-Moshi experience and
are Wave-1 requirements, not polish.**

| Tier | Cols | Layout | Promise |
|---|---|---|---|
| XL | ≥160 | rail + list + main + aux | everything |
| LG | ≥120 | list + main + aux | everything but the community rail |
| MD | ≥90 | list + main; aux is an overlay | every screen reachable; aux via overlay |
| MD-narrow | 72–89 | **collapsed list (18 cols: glyph + unread count, no names)** + main; aux overlay | channel context and agent presence never lost |
| SM | ≥60 | main only; list is a dialog (`<leader>k`) | every screen reachable via dialog |
| XS | 40–59 | main + composer; **all** navigation via palette/dialogs; timestamps abbreviate, author lines collapse to a prefix | read, post, mention, fleet, usage strip |

**Minimum supported is 40×16.** An earlier draft declared "minimum supported is
80×24" while also making SM (≥60) and XS (<60) Wave-1 requirements and demanding
`xs(50x20)` snapshots — i.e. it declared the phone tier both required and
unsupported, and nobody could tell whether XS was a support tier or a
don't-crash tier. §1.2 settles it: the phone is **one of the two primary reading
surfaces**, so XS is a product tier with the SLA in the table above, not a
degradation. Below 40×16 the app renders a single legible "terminal too small —
40×16 minimum" line and keeps running.

**MD-narrow exists because 72 columns is the everyday case.** Half of a 144-column
terminal is the single most common vertical split, and the draft's MD ≥90 → SM
≥60 jump dropped it to "main only; list is a dialog" — losing the channel list,
the agent rail, unread visibility, and presence glyphs at precisely the width
these operators work at. Losing channel *names* is much cheaper than losing
channel *context*, so the collapsed 18-column list keeps glyph + unread count.
MD's floor is set by measuring §3.4.1's content, not by round numbers, once the
mocks are redrawn at their true widths (§3.1).

Rendering into a zero-size rect is a no-op, never a panic. Per-breakpoint value
resolution (padding, label-vs-icon, abbreviated-vs-full timestamps) is table-
driven, not scattered `if (width < 80)` checks.

**Status-bar segments drop right-to-left in a declared priority order**, and the
first two never drop:

```
connection > pending-leader > mentions > unread > agent-usage-strip > context > help
```

The XL status bar carries six segments at 118 columns; without a stated order,
the segment that truncates on the phone could be the connection indicator — which
would fail §1.3 property 3 exactly where it matters most — or the pending-leader
indicator, which §3.7 calls out as the number-one way a keybinding grammar feels
broken. On XS the bottom row is therefore a **state line**, not the static hint
string an earlier draft's mock showed.

XS, drawn at 50×24:

```
┌────────────────────────────────────────────────┐
│ #engineering                       ●9  ⚡2  ⬤⬤ │
│ opus-5 · 58k/— · $0.21 · ⚡2 · ↻7              │
├────────────────────────────────────────────────┤
│ troy                                    14:02  │
│ can you take the 40008 diff rows?              │
│ ♥2 💯1                              ⤷4        │
│                                                │
│ claude-1 ⬤                              14:02  │
│ On it. The aux fetch is keyed by `#e`          │
│ over loaded ids, not by the time window.       │
│                                                │
│ ┌ diff · session.rs · +18 −4 ─── ⏎ ┐           │
│ └───────────────────────────────────┘          │
│                                                │
│ ── ● 4 new ─────────────────────────────────── │
│                                                │
│ matt                                    14:09  │
│ @claude-1 what's the 44200 cadence?            │
├────────────────────────────────────────────────┤
│ > ▁                                            │
├────────────────────────────────────────────────┤
│ ⬤⬤ │ ^X- │ 2 mentions · 12 unread              │
└────────────────────────────────────────────────┘
```

(`⬤⬤` = both links; `^X-` = a leader chord is pending. Both survive every tier.)

**Glyph width is a probed capability, not an assumption.** Nearly all of this
document's chrome — `● ○ ◐ ★ ♥ ▏ ▓ ─ │ ┌ └ ├ ┤ ⊙ → ⇧ ≠ ≤ …` — is East-Asian
**Ambiguous**: single-width under a Latin locale and **double-width** under
CJK-ambiguous settings, which tmux, iTerm2, and Ghostty all expose as a toggle
(and tmux's own width table differs from libc's). Every box drawn here misaligns
for such a user, and a T1 suite that byte-compares grids assuming width 1 would
never notice. `⚡` and `💯` are genuinely double-width and sit inside fixed-width
columns; `⬤` (U+2B24) carries the most important semantic in the app and is
missing from many default font stacks. The composer already mandates
grapheme-based display width (§3.3) — the chrome gets the same rigor:

- **An audited unambiguous glyph set** for all chrome, with the ambiguous
  characters above replaced or explicitly width-policied.
- **A startup probe**: emit the glyph, `ESC[6n` cursor-position round-trip, read
  the advance. Cheap, done once, and it settles the ambiguity for the session.
- **`BUZZ_TUI_ASCII=1`** selects a pure-ASCII fallback theme, which the XS tier
  defaults to.
- **T1 gains an ambiguous-wide snapshot dimension** (§5.3).

### 3.10 Theming

A closed semantic token set with the syntax palette **derived**, not
hand-specified. Base theme is **Catppuccin** to match Buzz — Latte (light) and
Mocha/Macchiato (dark) as *one* theme with two modes, honouring the terminal's
reported mode with an explicit lock command, exactly as the desktop and mobile
apps already pair Latte/Macchiato.

Token families: semantic (`primary secondary accent error warning success info`)
· text (`text textMuted selectedListItemText`) · surface (`background
backgroundPanel backgroundElement backgroundMenu`) · border (`border borderActive
borderSubtle`) · domain (`diff*`, `markdown*`, `syntax*`).

Disciplines that are review gates:

- **No literal hex in feature code.** Tokens only.
- **Neutral tones for large surfaces and chrome; high-chroma accents reserved for
  focus and selection.** This one rule is most of the difference between a TUI
  that looks designed and one that looks like a Christmas tree.
- **WCAG AA contrast is a CI test** over every (foreground, background) pair in
  the token table, per theme, in both modes.
- **Honour `NO_COLOR` and `TERM=dumb`**; pre-compute styles once at startup.
- **Per-community accent** — the community rail tints each entry so which
  community you are in is legible at a glance in a mirrored phone pane.
- **Per-user colour derived from the pubkey** — hash → hue, clamped to a
  theme-appropriate lightness/chroma band so it stays legible in both modes.
  Deterministic, so the same person is the same colour on both operators' boxes.

### 3.11 Attention

Focus-aware, with **explicit skip reasons** so "why didn't I get pinged" always
has an answer. Terminal focus is three-state (`unknown | focused | blurred`).
**`unknown` is never treated as `focused`** — a terminal that does not report
focus, or a session whose focus state went stale across detach/reattach, must
still deliver. For delivery decisions `unknown` resolves to `blurred`; only a
*confirmed* `focused` suppresses.

Event classes and defaults (delivery channels are §3.11's table below; `tmux` and
`journal` are the two that reach a detached session and a phone):

| Class | Default `when` | Default delivery |
|---|---|---|
| `dm` | blurred or unknown | tmux + journal + OSC 9 |
| `mention` | blurred or unknown | tmux + journal + OSC 9 |
| `agent_needs_input` | always | tmux + journal + bell (a blocked agent is blocked whether or not you are looking) |
| `agent_done` | blurred or unknown | tmux + journal |
| `channel_message` | never | — (opt-in per channel) |
| `error` / `connection_lost` | always | status-bar state, no bell |

**Delivery has four channels, and the tmux-native and file ones are the only two
that reliably work on the flagship surface.** An earlier draft specified "bell
plus OSC 9 (and `notify-send` when `DISPLAY` is set), degrading silently where
unsupported" — which on the actual target configuration degrades to *nothing*:
inside tmux OSC 9 is swallowed without `allow-passthrough`; the bell is consumed
by tmux's `monitor-bell`; the VPS has no `DISPLAY`; and on a phone mirroring a
tmux pane through Moshi, none of the three reach iOS. That makes
`agent_needs_input` — the orchestrator's single highest-priority event — a
promise kept only inside a process nobody is looking at. And a phone that cannot
be alerted is a read-only phone, which is a strictly worse Slack.

| Channel | Mechanism | Reaches |
|---|---|---|
| **tmux-native** (primary) | set the pane title / window name; let tmux's own activity + bell flags do the work | the indicator a tmux dweller actually sees, including across a *detached* session |
| **attention journal** (primary) | append one line per event to `$XDG_STATE_HOME/buzz/attention.jsonl` | anything the operator wires up — ntfy, Pushover, Moshi — in three lines of shell. This is the honest phone-notification answer and it costs nothing |
| OSC 9 | passthrough-gated | outer terminal when configured (`buzz-tui doctor` reports it) |
| bell / `notify-send` | best-effort | local desktop sessions |

Message text is sanitized before it leaves the process — strip ANSI, collapse
newlines, drop control characters, truncate by **grapheme count**. `notify()`
never throws and always returns a structured `{ok, skipped?: reason}`, and the
skip reason names the channel that was unavailable.

**`unknown` focus is treated as blurred for delivery — it fires.** Three-state
focus detection needs tmux `focus-events on` and is stale across detach/reattach,
while every `blurred` default above would otherwise silently never fire on a
terminal that does not report focus. Not-knowing must resolve toward delivering,
and §5.2 carries the T0 case.

The chat-specific rule: anything that would fire while the terminal is
*confirmed* focused *and* the relevant channel is the active pane is suppressed
by default.

---

## 4. Wave plan

> **Owner directive (Troy, 2026-08-04), binding on the wave plan below:** the
> TUI must mirror the desktop's full user journey, from first-run onboarding
> (relay, key create/import, community join) through general-course agent
> addition (harness discovery, agent creation, deploy). Agent creation/deploy
> and the onboarding wizard are pulled forward from Wave 3 into Wave 2. The
> harness preset catalog (desktop-compiled metadata) is mirrored manually and
> tracked as a named drift surface. Quality bar for every wave: "as clean and
> good as claude code/opencode while running Buzz perfectly." Every big
> milestone produces visual + technical dogfood artifacts (VHS + snapshots +
> live-relay capture-pane E2E) for the owner's human pass.


Full desktop parity is the destination [LOCKED], shipped in waves ordered by
**operator pain**, not by module size. `agents/` is 344 files and `messages/` is
229; neither of those numbers determines wave order. What determines it is: can
the two operators do their day's work in the terminal without switching to the
desktop?

### 4.0 Ordering principle

Each wave has one question it answers. A wave is done when the answer is yes and
its exit criteria (below) pass — not when its feature list is exhausted.

| Wave | Question |
|---|---|
| 1 | Can I read, post, and supervise agents from the box the agents run on? |
| 2 | Can I keep a whole workday in the terminal without the desktop? |
| 3 | Can I *administer* — agents, membership, moderation, deploys — from here? |
| 4 | Is anything left that I still open the desktop for, other than the hostile list? |

### 4.1 Wave 1 — fully specified

**Scope** [LOCKED]: channels/threads/posting + live streaming · @-mentions with
autocomplete (humans and agents) · agent activity viewer (observer feed + NIP-AM
usage: models, token burn, tool calls) · full-text search.

#### 4.1.1 Daemon deliverables

1. **Session layer** over `buzz-ws-client` implementing the constant table of
   §2.4/[D-4] verbatim: NIP-42 auth, subscription registry with per-channel
   `since` watermarks, `TwoGenDedup` at 12k, ping/pong half-open detection, the
   backoff ladder with DNS-brownout special case, REQ pacing at 125 ms with a
   drain budget of 1, and the gated-observer park queue at 256 with visible drop
   accounting.
2. **Identity**: ncryptsec load reading log-n from the header via
   `spawn_blocking`, `--identity-ncryptsec` + `--passphrase-stdin`,
   `--identity-credential` for the unattended systemd path,
   `POST /session/identity` (`{ncryptsec_path, passphrase}` only), NIP-OA auth-tag
   load/verify/expiry with the two signing entry points, env-var refusal,
   zeroizing storage, peercred check on accept, and `archiving: false` on
   `/health` while keyless (§2.5).
3. **Channel discovery + cache**: 39002 `#p=self` → `#d` uuids → 39000 batch, kept
   live from 44100/44101. SQLite at
   `~/.local/share/buzz/<hash>/cache.db`, `0600`.
4. **Timeline fetch — NIP-CW window [D-10]**: `#h` + `kinds` =
   `TIMELINE_KINDS` verbatim (`[9, 40002, 40008, 40099, 43001..43006, 48100]`)
   with `top_level: true`, `include_aux: true`, `include_summaries: true`, and
   the composite `(until, before_id)` cursor echoed from the previous page's
   `39006`. **`39006.has_more` is the sole exhaustion signal**; bounds-integrity
   failures discard the page rather than guessing. `39005` supplies thread
   summaries and reply counts. The **degradation branch** (no valid `39006` →
   clean standard filter, aux by `#e` over loaded ids, threads assembled
   client-side) is implemented in the same wave, because a downgrade that has
   never run is a downgrade that does not work. Getting this wrong is invisible
   until it silently isn't.
5. **Unread + read-state (NIP-RS)**: kind 30078 `d=read-state:<slot>`, NIP-44
   self-encrypted `{v:1, client_id, contexts}`; context keys bare
   `<channelUuid>` / `thread:<id>` / `msg:<id>`; the **hierarchical frontier**
   `effective(ctx) = max(merged[ctx], effective(parent(ctx)))` with the
   thread→channel parent link **derived from the event graph at evaluation time,
   never serialized**; max-wins merge across devices; 32 KB per slot, ≤8 slots,
   10k contexts, 7-day horizon for `msg:`/`thread:`, 5 s publish debounce.
   Unread counting gated by `isConversationalUnreadKind` so system/job/huddle rows
   never create phantom unreads.
6. **Mentions**: `/mention/candidates` and `/mention/inbox` over the roster cache.
7. **Search**: `/search` with always-set `kinds`, operator parsing, `409
   ambiguous_author`.
8. **Observer pipeline**: the 24200 subscription with the reference filter
   (`limit 1000`, `since now-300`), the **nine**-guard chain including the ±300 s
   freshness guard (§2.5), ciphertext-at-rest archive with idempotent
   `(agent_pubkey, seq, timestamp)` upsert [D-3], the byte-budgeted decrypt cache
   plus separate live-ring/archive paths and the 100-frame unknown-agent queue
   [D-3], `GET /agent/{pk}/activity` merging live + archived into **one** sorted
   deduplicated sequence, `GET /agent/{pk}/transcript` doing the ACP fold in the
   daemon, and `POST /agent/{pk}/control` for **cancel-turn and switch-model
   only** (§2.4).
9. **Ask-card projection**: parse `["ask", json]` off kind:9/40002 with
   `askCard.ts` validation verbatim (`v == 1`, `ASK_MAX_OPTIONS = 20`, option
   shape), expose it on the hydrated message, emit `agent.ask.open` /
   `agent.ask.answered`, derive the awaiting count, and serve
   `POST /message/{id}/ask` as a threaded kind:9 reply carrying the `broadcast`
   tag (§3.4.1).
10. **Fleet reduction**: `GET /agent/fleet` — per-agent state, current turn,
    elapsed, in/out tokens, per-model cost, context %, burn rate, and the
    repeated-identical-tool-call counter, sorted blocked-first (§3.4).
11. **Presence**: live from 20001, durable last-seen and cold-start from **40902**,
    with `unknown` distinct from `offline` (§2.4).
12. **NIP-AM 44200**: decrypt, validate, `GET /agent/{pk}/metric` **with
    `#p = self`** (the `ids` exemption does not apply — §2.4), `agent.metric`
    stream event.
10. **`/event`**: ndjson default, SSE by content negotiation, daemon-global
    monotonic `seq`, 10k ring, `?since=` replay, `stream.reset` on cursor
    aged-out, per-topic drop policy [D-5].
11. **OpenAPI 3.1 emission** + TS client generation, with an endpoint-not-in-spec
    build failure.

#### 4.1.2 TUI deliverables

Screens: main chat (§3.1) · thread docked/focused/detached (§3.2) · mention
picker (§3.3) · **agent fleet (§3.4)** · agent activity (§3.4.1) · search (§3.5) ·
palette (§3.6) · help overlay · which-key panel · community switcher · channel
switcher · `buzz-tui doctor` output (§3.7).

Systems: keybind table + mode stack + dialog stack with focus restoration ·
composer parts model with extmarks · **daemon-held per-channel drafts** ·
frecency ranking · prompt history · external-editor handoff · store-reconcile
streaming with 16 ms batched flush and catch-up mode · sticky-bottom scroll with
event-id-anchored unread divider · Catppuccin theming with derived syntax ·
responsive tiers XS→XL with the MD-narrow band · glyph-width probe + ASCII
fallback · attention with skip reasons and the journal sink · two-link connection
chrome.

**Drafts live in the daemon, not the TUI's state dir.** §2.1's own diagram shows
two TUIs on one daemon and §1.5 calls the second-client case real from day one —
so a per-front-end draft store means the same operator composing in
`#engineering` in pane 1 and pane 2 gets two silently diverging texts, and
whichever sends last wins. Drafts are per-*identity*, not per-front-end, which is
exactly what makes them different from frecency ([D-2] keeps that client-side for
the opposite reason: it is UI personalization). Cheap to put in the daemon, and
it removes a "where did my message go" report.

#### 4.1.3 Explicitly out of Wave 1

Reactions are **read-only** in Wave 1 (rendered, not authored) — the picker is
Wave 2. Message **edit, delete, and mark-unread** are Wave 2 and therefore carry
no Wave-1 binding (§3.7); they appear greyed in the palette with a `Wave 2` tag.
Rich-text marks are markdown source only. `@channel` is deferred pending a wire
representation (Q8). Huddle kinds 48101–48103 are Wave 4. No media, no forum, no
projects, no DM *creation* (existing DM channels read and post like any channel),
no moderation, no deploy, no workflows, no settings screen beyond `:set theme`,
`:set leader`, and `:set mouse`.

#### 4.1.4 Exit criteria

Wave 1 ships when all of these hold:

1. Both operators have used the TUI as their primary Buzz client for **five
   consecutive working days**, keeping a **tallied desktop-launch log with every
   launch's reason recorded**. The bar is not "no fallback for anything on the
   Wave-1 list" — scoped that way, falling back to the desktop ten times a day to
   add a 👍 or fix a typo *passes*, which is how an exit criterion gets negotiated
   rather than met. The log is the falsifiable version, and it doubles as the
   Wave-2 backlog: if the tally is dominated by one capability, that capability
   was mis-waved.
2. The T0+T1+T2 suites (§5) are green in CI and **required** on PRs touching the
   TUI or daemon.
3. Snapshot coverage exists for every screen × every tier, including the
   `xs`/`sm` rows and the `disconnected`, `auth-failed`, `unread-divider`, and
   `agent-streaming` states.
4. A daemon restart mid-session is invisible to the TUI beyond a brief chrome
   state change, and loses no read-state.
5. `GET /daemon` drop counters read zero across a normal working day; any
   non-zero value has an explained cause. **Daemon count and total daemon RSS are
   recorded in the same check** and stay within the §2.2 cap — an unbounded
   process population is the failure this fork has already had once.
6. The unread divider survives: reconnect burst, tier change 120→72→50→120,
   daemon restart, and a second client marking a different channel read.
7. A blocked agent reaches the operator's phone. Concretely: `agent_needs_input`
   writes to `attention.jsonl` and sets the tmux window flag, and one wired
   shell notifier delivers it to a phone while the tmux session is *detached*
   (§3.11). A phone that can read but cannot be alerted is not the flagship
   surface §1.2 claims.

### 4.2 Wave 2 — a whole workday

Question: can I stop opening the desktop for ordinary work?

- **Home inbox** (route `/`) with all 8 filters — mentions, threads, needs
  action, projects, drafts, reminders, focus, all. Three-pane list/detail is the
  most terminal-native shape in the entire app and it is the operator's morning
  screen. (The *agent* half of "needs action" already shipped in Wave 1 as the
  fleet screen, §3.4; Wave 2 generalizes it across non-agent sources.)
- **Reactions authoring** — quick-reaction row plus a fuzzy `:shortcode:` picker.
  Custom emoji render as literal `:shortcode:` text with the URL available on
  yank; the image is a desktop handoff.
- **DMs** — list, open (`build_dm_open`, kind 41010 — the relay replies with a
  `channel_id`; **not** NIP-17 gift wrap, or DMs will not interoperate with the
  rest of the product), hide, new-DM picker.
- **Forum channels** — 45001/45003, post cards, thread panel, permalink route.
- **Sidebar organization** — sections, stars, mutes, sort preference (all kind
  30078 `d`-tagged), context menu actions, `<leader>`-driven reorder in place of
  drag-and-drop.
- **Message editing/deleting/reply-to authoring**, thread follow/unfollow,
  mark-unread.
- **Channel management** — create, join, leave, archive, topic/purpose, canvas via
  `$EDITOR` handoff, channel browser with search-or-create.
- **Members sidebar** — list, search, per-member card, roster agent start/stop.
- **Presence and user status** (20001 / 30315) — set and display.
- **Reminders** (40007 / 30300) with a due-notification path.
- **Pulse** (kind 1 notes, kind 3 contacts) — read, publish, react, reply.
- **Local archive control** — save subscriptions by kind/scope. Arguably *more*
  valuable in a TUI than on the desktop: a grep-able SQLite mirror of relay
  history is exactly the terminal idiom.
- **Multi-community** — N daemons, community rail, per-community accent,
  aggregated unread.

Daemon additions: `/dm/*`, `/emoji`, forum endpoints, `/reminder`, `/note`,
`/status`, sidebar-preference read/write (30078 `d`-tagged), archive control.

### 4.3 Wave 3 — administration

Question: can I run the fleet from here?

- **Agent authoring** — persona (30175) / team (30176) / managed agent (30177)
  create and edit: identity, harness selection, LLM provider, model picker, env
  vars with missing/required detection, MCP servers, prompt sections (bodies via
  `$EDITOR`), respond-to policy.
- **Where-to-run / deploy** — the remote-first flagship. Run-target selection,
  provider config-schema rendering as a prompt sequence (including the
  Tailscale-decorated `oneOf` on `ssh_host`, degrading structurally to a plain
  text field when absent), remote harness discovery pinned verbatim onto the
  agent record at create time, remote model probe with the same env merge order
  the host's deploy uses, exclusive-harness refusal, and `recovery:
  {action:"open_url", url}` printed as an actionable URL.
- **Managed-agent lifecycle** — deploy/shutdown for provider-backed, spawn/stop
  for local, runtime rows per community, auto-start and auto-restart policy,
  managed-agent log panel.
- **Agent snapshots** — export/import as files and `buzz://` URLs (the avatar PNG
  rides along and is simply not rendered).
- **Moderation** — report (1984), ban/unban/timeout/untimeout (9040–9043),
  resolve-report (9044), the mod queue, the composer timeout banner with
  countdown, and the moderation-DM composer disable that **fails open**.
- **Community members + invites** — list, add, remove, role change, mint/claim
  invite, join-policy accept.
- **Workflows** — list, trigger, run trace, and the **46010 approvals inbox**
  (grant/deny with a note). Authoring is YAML via `$EDITOR`, not a form builder.
- **Agent memory (engrams)** — read, gated on the **declared-owner** half of the
  ownership check (§2.5).
- **Settings** — the panels that are forms and lists.

Also in this wave: the **`buzz-relay-session` extraction** [D-4] becomes a
blocker rather than a follow-up.

### 4.4 Wave 4 — the long tail

- **Projects / Buzz Git** (87 files): project and issue and PR lists, PR detail
  with files-changed, inline comments, reviews, merge, repo clone/push/pull/branch,
  commit detail, contribution graph as a braille/block heatmap, and
  "open a terminal in the repo" — which is *trivially better* in a TUI.
- **Channel templates**, **identity archive** (13535), **custom emoji set
  authoring**, **mesh-compute control surface**, **channel canvas** editing,
  **onboarding** (identity create/import, ncryptsec backup + restore test,
  machine onboarding, runtime install streaming), **notification preferences**,
  **huddle read-only state** (card, roster, lifecycle rows, transcripts) with a
  join handoff.

### 4.5 Wave-by-wave feature-count sanity

| Wave | Modules covered (of 30) | Approx. share of user-facing surface |
|---|---|---|
| 1 | messages (core), channels (read/read-state), search, agents (observer + metrics), relay lifecycle | ~35% |
| 2 | + home, forum, DMs, sidebar, pulse, reminders, presence/status, local-archive, communities, custom-emoji (shortcodes) | ~70% |
| 3 | + agents (authoring/deploy/lifecycle), moderation, community-members, workflows, agent-memory, settings | ~90% |
| 4 | + projects, templates, identity-archive, mesh, onboarding, notifications, huddle (read-only) | ~99% |

The residual ~1% is §4.6.

### 4.6 Terminal-hostile: the explicit handoff list

These never get a bad imitation and never get silence [LOCKED]. Each renders a
labelled placeholder plus a one-keystroke `open_in_desktop` (`<leader>D`) that
emits the exact deep link.

| Capability | What the TUI shows instead | Handoff link |
|---|---|---|
| Inline images / video in the timeline | `🖼 image · 1440×900 · 214 KB · ^X D open · y yank URL` | `buzz://message?channel=…&id=…` |
| `view_image` tool preview in a transcript | `🖼 view_image · <path> · ^X D open` | agent activity deep link |
| Agent card art (mint + view) | `card: minted 2026-07-14 · ^X D view` | agent profile |
| Avatars, animated avatars, webcam capture | initials + deterministic pubkey colour | profile settings |
| Composer image editor (crop/annotate) | `^X D edit image in desktop` | composer |
| Community icons | the community's accent colour + first letter in the rail | — |
| Custom emoji glyph | literal `:shortcode:` in the theme's accent | — |
| **Huddle audio** (start/join/mic/TTS/STT) | huddle card, participant list, lifecycle rows, and live transcripts — **read-only** | `buzz://huddle?channel=…` |
| Voice model download / pocket voices | settings row marked desktop-only | settings |
| Sound preview | — | settings |
| Contribution graph *(Wave 4 makes this ADAPTED, not hostile)* | braille heatmap | — |

Rules for the handoff, all three enforced:

1. **The placeholder states what it is** — dimensions, size, filename, path.
   `[image]` alone is a dead end. Key hints in a placeholder render the *actual
   current binding* for a §3.7 table entry (`^X D`, `y`), formatted by the same
   function the palette and help overlay use — never an invented bracket letter.
2. **The URL is always yankable** via OSC 52, because on a headless VPS "open in
   desktop" means "paste this into the desktop on my laptop".
3. **`open_in_desktop` degrades to yank** when no desktop is reachable, with a
   status line saying so — never a silent no-op.
4. **OSC 52 availability is probed at startup, and the fallback is a third
   mechanism — not a second no-op.** Rules 2 and 3 make yank the universal escape
   hatch and then make it the fallback for itself, which means that when OSC 52 is
   off *both* the primary path and its fallback silently do nothing — a dead end,
   which §1.3 property 2 forbids by name. And it is the thing most likely to be
   off: through tmux it needs `set-clipboard on` (plus passthrough when nested),
   the outer terminal must honour it, and Moshi/iOS may not. So: probe at startup
   (write-and-read-back where supported, else derive from the tmux/terminal
   config the way `buzz-tui doctor` does), and when unavailable render the URL in
   a **dedicated full-width, unstyled, no-wrap row designed for `tmux copy-mode`
   selection**, with a status line naming which mechanism is in use. `handoff-yank-unavailable`
   joins the §5.4 fixture list.

A capability that is neither implemented nor on this list is a **bug**, not a
gap. §5's fixture set includes a `handoff-coverage` scenario that walks every
TH-classified surface and asserts a labelled placeholder plus a working link.

---

## 5. Dogfood and test strategy

The governing rule: **assert on text, record video only for humans.** Video is
evidence, never an oracle.

### 5.1 Four tiers

```
T0  pure unit          ms      no terminal      pre-push
T1  headless render    ~1 s    fake terminal    pre-push + PR (the workhorse)
T2  tmux drive         ~5 s    real PTY         PR gate
T3  VHS record         ~30 s   real PTY + video PR artifact + release, never a gate
```

### 5.2 T0 — pure units

Everything in the design that is a pure function over data, tested with no
terminal at all:

| Target | Asserts |
|---|---|
| mention trigger detection | fires at start-of-input and after whitespace; **not** in `foo@bar`; closes on whitespace in the token; correct display offset with emoji/CJK before the `@` |
| display-width helpers | grapheme clusters, ZWJ emoji, CJK double-width, newline counts as width 1 |
| candidate ranking | prefix doubles score; frecency multiplier; roster outranks directory; empty query skips fuzzy |
| parts/extmark offsets | mid-sentence insert yields one space not two; duplicate mention updates the existing part |
| frecency | `freq/(1+ageDays)`; cap eviction; corrupt JSONL line dropped, not fatal |
| prompt history | duplicate guard; edited-entry move refusal; index clamping |
| keybind parse | `"none"`/`false` unbind; comma multi-bind; unknown key errors *naming the offender*; alias expansion |
| store reconcile | duplicate event id is a no-op; out-of-order insert lands sorted; delete/reaction reconcile in place |
| theme contrast | every token pair clears WCAG AA, per theme, both modes |
| search operator parse | token-boundary rule (`built-in:react` not parsed); `after:` inclusive local SOD; `before:` = SOD−1s; invalid value stays in FTS text |
| **read-state frontier** (Rust) | hierarchical `max(ctx, parent)`; graph-derived parent; max-wins merge; slot split at 32 KB; 8-slot cap; 7-day horizon prune; `msg:` grow-only |
| **timeline kind partition** (Rust) | content vs aux vs non-conversational; `TIMELINE_KINDS` verbatim (48100 only); no phantom unreads from 40099/43xxx/48100 |
| **NIP-CW window paging** (Rust) | a **full** page with `has_more: false` terminates; row count never used as an exhaustion signal; missing/duplicated/mis-bound `39006` discards the page; `has_more ⇔ next_cursor ≠ null` violation discards; a no-`39006` response triggers the *downgrade branch*, not a guess |
| **observer guard chain** (Rust) | each of the 9 guards rejects and increments its own counter; a frame whose `pubkey ≠ agent` tag is dropped; a frame `>300 s` skewed is dropped; a replayed in-window frame upserts idempotently rather than duplicating; unknown-agent frames queue to 100 then drop-with-count, and re-evaluate when the registry loads |
| **relay-session recovery** (Rust) | `requeue_observer_in_flight` restores unacked writes **ahead** of newly parked frames; every unacked frame is conservatively retried (NOTICE carries no event id); drop accounting under `GATED_OBSERVER_QUEUE_CAP` overflow; `n_sub_active` / `observer_control_sub_active` survive a reconnect; membership dedup uses strict-`<` |
| **ask card parse** (Rust) | mirrors `askCard.test.mjs` — malformed json → null; `v: 2` → null; `options: "nope"` → null; `> ASK_MAX_OPTIONS` → null; answer reply carries the `broadcast` tag |
| **turn liveness** (Rust) | running-minimum skew offset; retroactive correction; terminal tombstone beats late liveness; all-quiet prune pause |
| **44200 decode** (Rust) | `null` ≠ 0; `totalTokens` never derived; **context-window denominator absent → `—` and no bar**; cost suppressed as a single figure when >1 model in session; unknown `stopReason` → `unknown`; unknown fields ignored |
| **fleet reduction** (Rust) | blocked-first ordering is stable and not preference-driven; burn rate over a frozen clock; repeated-identical-tool-call counter fires at N and resets on a different call |
| ncryptsec round-trip (Rust) | a fixture produced by `create_backup_with_log_n` at `BACKUP_LOG_N` decrypts; log-n is read from the header, never assumed; decrypt runs off the async runtime |
| redactor superset (Rust) | `daemon_prefixes ⊇ provider_prefixes`; `ncryptsec1` covered; end-of-line secret with no delimiter is fully redacted |
| filter invariant (Rust) | no filter is emitted without explicit `kinds`; `/agent/{pk}/metric` carries `#p = self` |
| attention routing | `unknown` focus fires (treated as blurred); every event appends one `attention.jsonl` line; skip reason names the unavailable channel |
| composite cursor (Rust) | same-second events neither skipped nor repeated across a page boundary |

Gate: `just tui-test-unit` + `cargo test -p buzz-daemon`, both in the existing
pre-push hook alongside the other fast suites.

### 5.3 T1 — headless render snapshots

Render the component tree to an in-memory cell buffer at fixed dimensions;
snapshot the **text grid**, not pixels.

Determinism requirements — a snapshot suite without all six is worthless:

1. **Frozen clock** — `BUZZ_TUI_FIXED_TIME`, all timestamps from an injected clock.
2. **Frozen randomness** — seeded PRNG for anything sampling.
3. **No animation** — `BUZZ_TUI_NO_ANIM=1` pins spinners to frame 0.
4. **Fixed dimensions** per snapshot, declared in the test name.
5. **Fixed theme, `LANG=C.UTF-8`, `TZ=UTC`.**
6. **Fixture-backed daemon** — T1 never touches a network.

Matrix: every screen × every tier × both glyph-width policies.

```
snapshots/<screen>/<tier>[.ambiguous-wide].txt
  screens: channel-list · timeline · thread-docked · thread-focused ·
           composer-empty · composer-mention-open · palette · which-key · help ·
           search-results · community-switcher · unread-divider ·
           agent-fleet · agent-activity · agent-streaming · agent-ask ·
           usage-strip · usage-pane · handoff-placeholder ·
           handoff-yank-unavailable · doctor ·
           disconnected · rate-limited · auth-failed · keyless-archiving-off
  tiers:   xs(50x20) sm(70x24) mdn(80x28) md(100x30) lg(140x40) xl(180x50)
  width:   default | ambiguous-wide   (§3.9 glyph policy)
```

Three matrix notes:

- The `xs`/`sm` rows are the ones everyone skips and they are the *phone* rows
  here. `mdn` is the 72–89 band (§3.9) — the everyday tmux split, which had no
  tier at all before this revision.
- `usage-strip` exists at **every** tier including `xs`; `usage-pane` exists only
  where aux does (`md` and up). Demanding a `usage-pane × xs` snapshot, as an
  earlier draft did, is a snapshot of undefined behaviour.
- `agent-permission` is renamed `agent-ask`, matching §3.4.1's actual mechanism,
  and its fixture covers both routings: `askOwner` (actionable) and `Auto`
  (informational, already answered, no affordance).

`BLESS=1 just tui-test-render` rewrites; the diff is reviewed like any other
diff. Add a **shadow-run** check: two runs of the same input must produce
byte-identical buffers.

Gate: required on every PR touching the TUI.

### 5.4 Fixture protocol

The TUI's only coupling to the backend is a URL, which is what makes the whole
pipeline testable:

```
BUZZ_TUI_FIXTURE=/path/scenario.jsonl   → in-process fake transport
BUZZ_DAEMON_SOCKET=/path/x.sock         → real daemon
```

A scenario is ordered JSONL of `{atMs, kind, payload}` replayed on the frozen
clock: an initial state snapshot, then the event stream. Author first:

| Scenario | Exercises |
|---|---|
| `empty` | cold start, no communities |
| `seeded-basic` | 3 channels, ~40 messages, 2 unread |
| `mention-burst` | 200 events in 100 ms → 16 ms coalescing + catch-up mode |
| `agent-stream` | token-by-token agent reply → stable message identity from first token |
| `observer-turn` | full turn: prompt → thought → read → shell → edit → ask → usage → complete |
| `observer-drop` | gated-observer queue overflow → visible drop counter |
| `observer-replay` | a captured frame re-delivered in-window and out-of-window → guard 0 drop, idempotent upsert, no duplicate row |
| `ask-askowner` / `ask-auto` | actionable ask card answered via a threaded broadcast reply; and the `Auto` case rendering as informational with no affordance |
| `fleet-triage` | 6 agents, one blocked, one looping → blocked-first order, `↻ ×N` counter |
| `window-downgrade` | a relay serving no `39006` → downgrade to the standard filter, not a guessed exhaustion |
| `keyless-daemon` | daemon running without an identity → `archiving: false` surfaced in chrome |
| `handoff-yank-unavailable` | OSC 52 absent → copy-mode-selectable URL row, not a silent no-op |
| `edit-delete-react` | late NIP-09 delete + late reaction over an old visible message → aux-by-`#e` |
| `reconnect` | stream drop, backoff, replay of seen ids → idempotent apply + divider survival |
| `stream-reset` | cursor aged out of the ring → invalidate-and-refetch, not a silent gap |
| `auth-fail` | NIP-42 rejection → distinct surface with remediation |
| `rate-limited` | write blocked with countdown; typing dropped, observer frames parked |
| `handoff-coverage` | every TH surface renders a labelled placeholder + working link (§4.6) |
| `readstate-multidevice` | a second client moves a marker → max-merge, no clobber |

**The same fixture files drive T1, T2, and T3.** That single-source property is
what keeps the tiers from disagreeing.

### 5.5 T2 — tmux drive (also the dogfood loop)

`tmux` is a better *gate* than VHS: fast, headless, yields text — and it is
exactly how the operators run the app, so the test harness and the product share
a substrate.

```bash
tmux new-session -d -s buzztui -x 120 -y 40 \
  "BUZZ_TUI_FIXTURE=$FIX BUZZ_TUI_NO_ANIM=1 TZ=UTC ./buzz-tui"
until tmux capture-pane -p -t buzztui | grep -q '#general'; do sleep 0.1; done
tmux send-keys -t buzztui 'C-p'
tmux send-keys -t buzztui 'mark'
tmux capture-pane -p -t buzztui > /tmp/palette.txt
grep -q 'Mark channel as read' /tmp/palette.txt
tmux kill-session -t buzztui
```

Rules, each learned the hard way elsewhere:

- **Never `sleep N` as a readiness proxy** — poll `capture-pane` for a known
  marker with a hard timeout. Blind sleeps are the number-one source of flaky TUI CI.
- **Fixed pane geometry** at creation; never resize mid-test unless resize is the
  thing under test.
- **Isolated per-run `XDG_STATE_HOME`** so the JSONL stores (frecency, history,
  drafts) start empty and parallel runs cannot corrupt each other.
- **`capture-pane -e`** when the assertion is about colour ("the unread channel
  renders in the accent token").
- **`capture-pane -p -S -`** for scrollback assertions.
- **Always kill the session in a `trap`** — orphaned tmux sessions on a VPS are
  how this suite becomes a resource leak.

Gated sequences (interactions, not stills):

| Sequence | Asserts |
|---|---|
| type `@ma` | popup opens, `@matt` first, extmark styling applied |
| `@c` then `alt+2` | inserts the second row regardless of frecency order — deterministic pick |
| `⇥` on a candidate | **completes** (alias of `⏎`); `ctrl+s` is what scopes |
| `ctrl+c` mid-compose | first press clears the composer and does **not** exit; second within 2 s exits |
| two TUIs within 50 ms | exactly one daemon pid exists afterwards (§2.3) |
| resize 120→72→50→120 | MD → MD-narrow → XS → MD; collapsed list retains unread counts at 72 |
| `<leader>` then wait | which-key lists chat verbs |
| `<leader>` then `backspace` | pending token pops, panel closes |
| `ctrl+p` → filter → `enter` | dispatches, closes, **focus returns to composer** |
| open dialog → `escape` | closes top of stack only; focus restored |
| scroll up during a stream | sticky-bottom releases; new messages do not yank the viewport |
| `<leader>u` after a burst | divider anchored to event id, survives the burst |
| kill the fixture transport | status bar shows reconnecting + countdown, not silence |
| ask card → focus → `⏎` | a threaded **broadcast reply** is published (not a control frame); the card resolves to "Approved (allow_once)" |
| ask card under `Auto` routing | renders informational; no key answers it |
| second client marks read | first client's unread updates without a clobber |
| draft in pane 1, switch to pane 2 | same draft text — drafts are daemon-held, not per-front-end |
| `agent_needs_input` while detached | `attention.jsonl` gains a line and the tmux window flag is set |

Gate: required on PRs touching `buzz-tui` or `buzz-daemon`.

### 5.6 T3 — VHS recording

Evidence and docs, never a gate. Per-run artifact contract:

```
artifacts/tui/<run-name>/
  capture.tape   exact tape — required for reproduction
  vhs.log        stderr
  capture.mp4
  snapshot.png   frame at --snapshot-second via ffmpeg
  run_meta.json  {status, duration_seconds, vhs_exit_code, fixture_exit_code,
                  snapshot_status, video_exists, snapshot_exists,
                  video_duration_seconds, fixture, tier, commit, tui_version}
  fixture.jsonl  the scenario replayed
```

Tape hygiene: `Set TypingSpeed 0ms` (typing delay is a determinism hazard) and a
`Hide / Ctrl+C / Show` trailer so the quit keystroke is not in the recording.

**Strictness is mandatory in CI**: a passing MP4 is *not* success when the
fixture failed to load. Fail the run if the fixture did not load and fail it if
snapshot extraction produced no PNG.

**Screenshot-distinctness gate**, verbatim from the desktop's hard-won rule:

```bash
shasum -a 256 artifacts/tui/*/snapshot.png | awk '{print $1}' | sort | uniq -d
# any output = two captures took the same picture = fix the tape, do not post
```

**PR posting** uses `scripts/post-screenshots.sh` (per-developer
`agent-screenshots/<username>` branch, commit-SHA-immutable URLs). Never
`buzz upload` or relay media URLs — they fail through GitHub's camo proxy. Run
`scripts/check-pr-image-urls.sh` on hand-edited PR markdown. Delete superseded
screenshot comments after reposting.

### 5.7 How Claude-orchestrator agents develop and test this in-session

This is the part that makes the TUI unusually pleasant to build with agents: the
product's own substrate (`tmux`) and its test substrate are the same thing an
agent can already drive.

**The in-session loop:**

1. **Bun dev, no build step.** Development runs on Bun directly [LOCKED], so a
   TS edit is live on the next launch with no compile. The daemon is a `cargo
   build -p buzz-daemon` of ~34 s against a warm cache and is rebuilt only when
   the protocol surface changes — most sessions never touch it.
2. **Agents assert on text, not pixels.** `tmux capture-pane -p` returns a string.
   An agent can read the screen, diff it against an expectation, and iterate
   without a human in the loop and without a screenshot pipeline. This is the
   single biggest reason a TUI is more agent-testable than the desktop app.
3. **Fixtures over live relays.** An agent working on the mention picker sets
   `BUZZ_TUI_FIXTURE=seeded-basic` and never needs Postgres, Redis, a relay, or a
   key. Deterministic, hermetic, and safe to run in parallel across a swarm.
4. **Parallel isolation is per-run env, not per-repo worktree.** Each agent run
   gets its own `XDG_STATE_HOME`, its own tmux session name, and its own fixture
   copy. Multiple agents can drive multiple TUIs on one box simultaneously —
   which matters because this repo's convention is two agents per repo.
5. **Snapshot bless is the review artifact.** An agent making a visual change
   runs `BLESS=1`, and the PR diff *is* the before/after in reviewable text. No
   image comparison, no camo proxy, no upload step.
6. **A `just tui-drive <fixture> <keys...>` helper** wraps the launch/poll/send/
   capture/kill dance in one command so agents do not each reinvent the readiness
   poll (and get the `trap` cleanup for free). This is the single highest-leverage
   piece of tooling to build first — before any screen.

**The multi-agent dogfood loop** (and the reason the second-client-attach design
is not academic): one agent drives a TUI attached to a daemon while a *second*
agent posts into the same channel through `buzz-cli`. That exercises live
streaming, unread counting, read-state merge across clients, and mention
delivery against a real relay — the exact multi-agent E2E pattern `TESTING.md`
already describes, with the TUI as one more participant.

**What agents must not do:** run T3/VHS in-session (slow, produces binaries an
agent cannot evaluate), or `sleep`-and-hope instead of polling for a readiness
marker. Both are in the PR review checklist.

---

## 6. Repo, build, and release layout

### 6.1 Recommendation: in the fork, as two new top-level units

```
block/buzz fork (troyhoffman-oss/buzz), branch remote-first
  crates/buzz-daemon/          NEW Rust crate, in the root workspace
  tui/                         NEW top-level dir, Bun/TypeScript
    package.json               NOT in pnpm-workspace.yaml (see below)
    src/…
    test/{unit,render,tmux}/
    fixtures/*.jsonl
  .github/workflows/tui-release.yml   NEW, modelled on provider-release.yml
  Justfile                     + tui-dev, tui-drive, tui-test-{unit,render,tmux}
```

**Why in the fork rather than a separate repo** — five reasons, in order of
weight:

1. **The daemon must build against `buzz-ws-client` and `buzz-sdk` at the same
   revision as the relay it talks to.** Those crates are not published to
   crates.io. A separate repo means either a git dependency pinned to a SHA
   (which silently rots and turns every protocol change into a two-repo dance) or
   vendoring (worse). In-repo, `cargo` resolves them by path and a breaking change
   in `buzz-sdk` breaks the daemon build **in the same PR that caused it**.
2. **Upstream submission is the point.** This fork exists to feed `block/buzz`.
   A TUI in a separate repo is a fork artifact forever; a `crates/buzz-daemon` +
   `tui/` in the tree is a PR when the time comes. The daemon in particular is
   plausibly upstream-valuable on its own — `buzz-cli` and any future non-desktop
   client want exactly the same session layer.
3. **CI already exists.** `ci.yml` builds the workspace, `provider-release.yml`
   is a proven fork-owned tag-triggered release, and hermit already pins the
   toolchain. A separate repo re-creates all of that.
4. **The quality gates and conventions apply as-is** — `git commit -s` for DCO,
   `just ci`, biome, the pre-commit/pre-push hooks.
5. **Discoverability for the two users.** One clone, one `just`.

**Why `tui/` is a top-level dir and NOT a pnpm workspace member.** The repo's
`pnpm-workspace.yaml` lists `desktop`, `web`, `admin-web` — all pnpm+Node+Vite.
The TUI is Bun-first [LOCKED] with `bun.lockb` and a `bun build --compile`
release path. Folding it into the pnpm workspace would mean two package managers
sharing one lockfile graph, pnpm hoisting decisions affecting Bun resolution, and
`@opentui/core`'s prebuilt platform binaries (8 triples via
`optionalDependencies`) landing under pnpm's symlinked store where Bun's resolver
does not expect them. Keeping `tui/` outside the pnpm workspace is one line in a
config and removes an entire class of "works on my machine". The root
`package.json` gains nothing; `just tui-*` recipes are the entry point.

**Alternative considered and rejected: separate repo.** The only real argument for
it is that OpenTUI's pre-1.0 churn would then not touch this repo's dependency
surface — but the TUI's dependencies are already isolated by not being in the
pnpm workspace, so that argument buys nothing that the directory boundary does
not already buy, and it costs the path-dependency property in (1), which is the
load-bearing one.

### 6.2 CI workflows needed

Three, all on free public GitHub runners [LOCKED].

**(a) `ci.yml` additions** — required checks on PRs touching `tui/` or
`crates/buzz-daemon/`:

```yaml
tui-check:      # ubuntu-latest, path-filtered
  - bun install --frozen-lockfile
  - bun run typecheck
  - biome ci tui/
  - just tui-test-unit          # T0
  - just tui-test-render        # T1, snapshots
  - just tui-test-tmux          # T2, needs only tmux
  - just tui-check-boundary     # §6.4 disposability gate
  - just tui-check-mocks        # every fenced mock's measured width == its label
daemon-check:   # folds into the existing rust jobs
  - cargo clippy -p buzz-daemon -- -D warnings
  - cargo test -p buzz-daemon
  - just daemon-spec-check      # OpenAPI regenerated == committed, else fail
```

`daemon-spec-check` is what makes "adding an endpoint without adding it to the
spec is a build failure" real, and it also catches TS-client drift, because the
generated client is committed and regenerated in the same step.

**(b) `tui-release.yml`** — tag-triggered on `tui-v*`, modelled directly on the
fork's `provider-release.yml`, which is a working, reviewed pattern in this
repo:

- Push-triggered on a tag (so the workflow is read from the pushed ref and needs
  no default-branch registration on the fork).
- **No** `if: github.repository == 'block/buzz'` guard — every upstream release
  workflow carries one and silently skips on a fork; this file is fork-owned and
  must run there.
- Matrix: `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`
  [LOCKED]. Two jobs: `daemon-*` (cargo, `Swatinem/rust-cache`, hermit for the
  pinned toolchain — never `dtolnay/rust-toolchain`, it shadows the pin) and
  `tui-*` (`oven-sh/setup-bun`, `bun build --compile --target=<triple>`).
- A **shared packaging script** (`.github/scripts/package-tui.sh`) so targets
  cannot drift in naming or checksum format — the same reasoning that produced
  `package-provider.sh`. macOS runners have `shasum` not `sha256sum`; the script
  handles both and emits the identical `<hex>  <name>` line the publish job reads
  back with `sha256sum -c`.
- A publish job that re-verifies **every** checksum after the round trip through
  artifact storage and asserts an exact artifact count, so a silently dropped
  artifact fails the release rather than producing a partial one.

**(c) `tui-artifacts.yml`** — optional, `workflow_dispatch` + nightly: the T3 VHS
suite (every scenario × the `md` tier), a contact sheet, tarball uploaded as a
workflow artifact. **Never a PR gate.** Also runs the **fixture drift check**:
replay each scenario against a real daemon plus an ephemeral relay and diff the
resulting event stream against the recorded fixture. That check is the only thing
standing between a green fixture-based suite and a TUI that no longer matches the
protocol; a failure there is a fixture-regeneration task, not a TUI bug.

### 6.3 Release artifact shapes

Per tag `tui-v<semver>`, six binaries plus six checksums:

```
buzz-tui-x86_64-unknown-linux-gnu        + .sha256
buzz-tui-aarch64-apple-darwin            + .sha256
buzz-tui-x86_64-apple-darwin             + .sha256
buzz-daemon-x86_64-unknown-linux-gnu     + .sha256
buzz-daemon-aarch64-apple-darwin         + .sha256
buzz-daemon-x86_64-apple-darwin          + .sha256
```

**[D-9] Ship the pair, and have the TUI carry the daemon as an embedded
fallback.** The `buzz-tui` binary embeds the matching-platform `buzz-daemon` as an
asset; on spawn (§2.3) it looks for `buzz-daemon` on `PATH` first, and if absent
materializes the embedded copy into
`~/.local/share/buzz/bin/buzz-daemon-<version>` and execs that.

**Materialization is atomic and verified**, because the naive write-then-exec has
both a TOCTOU and a version-shadowing hazard — and on a box that by §1.2's own
premise runs arbitrary AI agents under the same uid with shell access, "a
concurrent same-uid process could replace the file between write and exec" is a
low bar, not a theoretical one. Two TUIs materializing at once can also interleave
writes and exec a truncated binary. So:

- Write to `buzz-daemon-<version>.<random>` in the same directory, `fchmod 0700`,
  `fsync`, then **`rename(2)`** into place — atomic, and a concurrent
  materializer's rename harmlessly overwrites identical bytes.
- **Verify a build-time-embedded sha256** of the asset before exec; refuse rather
  than exec on mismatch.
- The `PATH` lookup **must resolve to an absolute path** and must reject a
  `buzz-daemon` found in a group- or world-writable directory. "`PATH` takes
  precedence" is an injection vector otherwise.

Rationale: "invisible infrastructure" [LOCKED] is incompatible with "download two
files and put both on your PATH". Embedding makes the single-file promise literal
— `curl` one binary, run it, it works. The separate `buzz-daemon` asset still
ships because the VPS install (§6.5) wants to run it under systemd without a TUI,
and because a mixed-version setup (new TUI on a laptop, older daemon on the VPS)
must be resolvable by upgrading the daemon alone. `PATH` taking precedence means
an operator who manages the daemon explicitly is never surprised by an embedded
copy shadowing it.

Cost: roughly doubles the linux/mac download (~15–25 MB → ~40–50 MB). At two
users that is free; the ergonomics are not.

Release notes are generated the same way `provider-release.yml` does it — the
checksum block inline, the upstream/fork provenance stated, `--prerelease` until
Wave 1's exit criteria pass.

### 6.4 Enforcing "the front end is disposable"

This is [LOCKED] intent, so it gets a mechanical gate rather than a convention. A
CI check (`just tui-check-boundary`) fails the build if `tui/src/` contains:

- any Nostr, secp256k1, NIP-44, bech32, or nsec/npub-decoding dependency;
- any bare event-kind integer in the 4-digit-and-up range (kinds are daemon
  vocabulary; the TUI receives `type: "message.new"`, never `kind: 40002`);
- any hand-written request struct for a daemon endpoint (the client is generated
  and committed);
- any decode of a pagination cursor;
- any literal hex colour;
- any `{nsec}` field on the generated `POST /session/identity` client (§2.5 —
  the form does not exist, and a regenerated client that grows one is a spec
  regression);
- any hardcoded key string in a handler — every advertised key resolves through
  the §3.7 table (this is what makes the bracket-letter grammar unable to
  reappear).

The test for whether this design succeeded: a `ratatui` front end can be written
against the same daemon and lose **zero** protocol work. If any of the above
appears in the TS, that stops being true.

### 6.5 Install shapes

| Where | Shape |
|---|---|
| **VPS (agent host, primary)** | `buzz-tui` + `buzz-daemon` in `~/.local/bin`. Daemon under `systemd --user` with `--idle-timeout 0` and **`--identity-credential buzz-passphrase`** (§2.5 path 3) so the observer archive is genuinely always on — it survives a 03:00 reboot with no human present, which the stdin-only paths do not. TUI launched in `tmux`, attaches to the running daemon. |
| **Laptop → VPS** | `ssh -L` forwarding the daemon's UDS into a **`0700` per-user directory, never `/tmp`**: `mkdir -p -m 700 "$XDG_RUNTIME_DIR/buzz/fwd" && ssh -nNT -L "$XDG_RUNTIME_DIR/buzz/fwd/<hash>.sock":/run/user/1000/buzz/<hash>.sock host`, then `buzz-tui --socket "$XDG_RUNTIME_DIR/buzz/fwd/<hash>.sock"`. **The trust boundary here is the SSH session, not peercred** (§2.5): `ssh -L` creates the local socket with the process umask, so a world-writable parent directory hands a fully authenticated Buzz session — observer plaintext included — to anyone else on the laptop. `buzz-tui --socket` refuses a socket whose parent directory is group- or world-writable. No TCP listener ever opens. |
| **Phone** | Nothing. Moshi mirrors the VPS `tmux` pane. This is why XS tier is a Wave-1 requirement. |
| **Laptop standalone** | Single `buzz-tui` binary; it spawns its embedded daemon. |

---

## 7. Risks and open questions

### 7.1 Risks with mitigations

**R1 — OpenTUI is pre-1.0 and openly mid-migration at the JS/native seam.**
*Mitigation:* the disposable-front architecture, made mechanical by §6.4. If
churn becomes intolerable, a ratatui front is written against the same daemon and
zero protocol work is lost. Concretely: pin `@opentui/*` to exact versions (no
`^`), upgrade deliberately in their own PRs with the T1 snapshot diff as the
review artifact, and keep a running note of every OpenTUI-specific workaround
(the settle-through-an-effect hop, the 50 ms anchor poll) **out of the UX spec**
so the spec stays portable. A frame-loop architecture does not need either.

**R2 — `bun build --compile` caveats.** Known constraints: the produced binary
is large (Bun runtime is embedded); cross-compilation from one host to another
target works for the JS but **native `.node`/dylib addons must match the target**,
and `@opentui/core` ships exactly such prebuilt binaries per triple. *Mitigation:*
build each target on its own runner (linux on `ubuntu-latest`, both mac targets on
`macos-latest` with the x64 one as a documented cross, mirroring
`release.yml`'s `release-macos-x64` convention) rather than cross-compiling all
three from linux; assert in the packaging script that the produced binary is the
expected architecture (`file` output check, same as `package-provider.sh`); and
add a smoke job that actually **runs** each artifact with `--version` on its own
runner before publish. A binary that builds and does not run is the failure mode
here, and it is cheap to catch.

**R3 — Observer decrypt key handling.** The daemon holds the owner secret and
decrypts every agent's telemetry — prompts, file contents, shell output. That is
the single highest-value target on the box. *Mitigations, layered:* ciphertext at
rest [D-3]; `0600` file / `0700` dir; `zeroize` on all key material; UDS-only with
a peercred check; no environment-variable key path at all; redaction on both the
log sink and provider stderr; and the nine-guard chain (including the ±300 s
freshness window that closes replay on an ephemeral kind) so a forged frame cannot
reach a client even if it decrypts. *Residual:* an attacker with the operator's
uid on the box has the socket, and therefore has everything. That is the correct
and intended boundary — the same boundary the desktop's keyring has — and it is
stated here so nobody mistakes it for an oversight.

**R4 — Two runtimes to ship.** [D-9]'s embedding hides it from the user but not
from us: a release is two toolchains and two artifact families. *Mitigation:* the
shared packaging script and the exact-artifact-count assertion in the publish job,
both already proven in `provider-release.yml`.

**R5 — The read-state port is the highest-risk piece of protocol work in the
project.** Encrypted slots, a hierarchical frontier with a graph-derived parent
resolver, multi-device max-merge, and hard size limits. Get it wrong and unread
counts diverge *silently* — the worst failure shape. *Mitigation:* it is Rust, in
the daemon, with the densest T0 suite in §5.2, plus the `readstate-multidevice`
fixture, plus a Wave-1 exit criterion that specifically tests a second client
moving a marker.

**R6 — Upmerge friction.** The fork must keep merging `block/buzz`. *Mitigation:*
the TUI and daemon are **purely additive** — one new crate, one new top-level
directory, one new workflow, plus Justfile recipes. [D-4]'s decision not to
refactor `relay.rs` is precisely the choice that keeps it additive; the extraction
happens later as an upstream-submittable PR rather than as a fork-local
divergence.

**R7 — Wave 1 is large, and this revision made it larger.** Five flagship
surfaces plus a new daemon, with the session layer re-estimated at ~1,500–2,000
lines ([D-4]) rather than ~600. *Mitigation:* the exit criteria (§4.1.4) are
behavioural, not feature-count based; the daemon endpoint set is explicitly scoped
to the wave (§2.4); and the additions that came out of review are mostly
*reductions over data the daemon already folds* (the fleet screen, the usage
strip, the loop counter) rather than new subsystems. The ordered build sequence
is: `just tui-drive` helper → daemon session layer + `/event` → channel list and
NIP-CW timeline read → composer and send → mentions → **agent fleet** → agent
activity → search. Each is independently demoable, so a slip is visible early
rather than at the end. If one must slip, it is the single-agent transcript
(§3.4.1) and not the fleet screen — the fleet screen is the one that decides
whether the desktop stays open.

### 7.2 Open questions

**Q1 — Inline mode.** Stable chrome with content scrolling in the terminal's
*native* scrollback would be genuinely differentiating for a chat client (mouse
selection, real scrollback, `tmux copy-mode` over history). OpenTUI does not
appear to support it. This is the strongest single argument for the ratatui hedge
and is worth a timeboxed spike before Wave 1 locks its shell. **Not a blocker** —
alt-screen with in-app scroll is acceptable — but if it turns out to be
achievable it changes the scrollback and yank design.

**Q2 — `@opentui/ssh`.** Serving the TUI itself over SSH is a striking fit for a
remote-first agent platform and would make T2/T3 trivially remote. It also
partially overlaps the Moshi story. Worth a spike after Wave 1; explicitly *not*
in the wave plan, because the `tmux` path already works and adding a second remote
access mechanism before the first is proven is scope we do not need.

**Q3 — Bun vs Node for the shipped runtime.** [LOCKED] says `bun build --compile`
for releases and Bun for development. The open part is whether a *Block-signed*
distributable would ever need the Node path instead. Irrelevant while this is a
fork artifact for two operators; becomes a live question at upstream-submission
time, and the answer likely depends on Block's OSS supply-chain policy regarding a
pre-1.0 dependency (`@opentui/core`) on a flagship client's critical path.

**Q4 — OpenAPI vs `ts-rs` for the client contract.** This document assumes
OpenAPI 3.1 (matching opencode, and what `daemon-api.md` specifies). `ts-rs` is
simpler and has no HTTP-spec ceremony, but produces types only — no client, no
route contract, and no non-TS consumers ever. OpenAPI is the right call *if* a
second client (a web UI, a scriptable client, `buzz watch`) is ever wanted, which
the daemon design explicitly wants. Recommend OpenAPI; flagging that the ceremony
is real and that `ts-rs` remains a legitimate downgrade if the spec tooling
becomes a drag.

**Q5 — Per-channel drafts *and* a stash, or just drafts?** The desktop has
per-channel drafts (they are in `resetCommunityState()`); opencode has a single
stash. This document specifies per-channel drafts in Wave 1 and puts the stash on
the palette-only long tail. If the operators find themselves parking multiple
drafts per channel, the stash gets promoted. Cheap to defer, cheap to add.

**Q6 — Does the TUI ever need to render Blossom media at all?** §4.6 says no
images, ever. But *text* attachments — a pasted log, a patch file, a JSON blob —
are terminal-native and currently sit behind the same media path as images. Worth
deciding in Wave 2 whether text-typed blobs get inline rendering with a size cap,
rather than being lumped in with the image handoff.

**Q7 — Does the daemon need a `/count` passthrough?** Cheap at the relay,
meaningless when stale, and no Wave-1 screen needs it. Named here only so that
"the daemon should expose everything the relay does" is refused explicitly rather
than by omission.

**Q8 — What is `@channel` on the wire?** Removed from Wave 1 ([D-2], §3.3)
because it has no protocol representation: nothing in `buzz-sdk`, the relay, or
`desktop/src` handles it, and client-side expansion to N `p` tags works at 27
members and fails at 51 against `MENTION_CAP`. The right shape is almost
certainly a marker tag the *relay* expands, with fan-out accounted relay-side —
which is a protocol proposal, not a client feature. Wave 2 at the earliest.

**Q9 — Where does workflow and agent-team structure surface?** The fleet screen
(§3.4) answers "which agent needs me" for a flat roster, but an orchestrator
watching a fan-out sees N unrelated transcripts with no tree: workflow runs and
the parent/child structure of agent teams (kind 30176) are both Wave 3. The fleet
table is deliberately flat in Wave 1 — a tree view is a different screen, not a
column — but if the operators' dominant pattern turns out to be fan-out rather
than independent agents, this is the first thing to pull forward.

### 7.3 Review findings deliberately not adopted

Recorded rather than dropped, each with the reason.

- **"Delete `@channel` *or* specify it now."** Adopted the deletion, declined the
  "specify it now" branch. Defining a mention-fanout primitive is a protocol
  change with relay-side fan-out accounting — it belongs in a NIP proposal
  reviewed on its own merits, not settled inside a client design doc as a Wave-1
  side effect. Tracked as Q8.
- **"Import `buzz_backend_ssh::protocol::redact` so there is one definition."**
  Taken as the stated long-term goal but not as the Wave-1 mechanism (§2.5). The
  daemon needs `ncryptsec1` coverage now, and adding a prefix to a shared
  upstream crate is an upmerge-surface change in exactly the file class R6 exists
  to keep additive. The superset **test** gets the same safety — an upstream
  addition the daemon has not adopted fails the build — without touching upstream
  code. Import happens when `ncryptsec1` lands upstream on its own PR.
- **"Extract `buzz-relay-session` before Wave 1 ships."** Not proposed by either
  review, but re-affirmed here against the enlarged [D-4] estimate: even at
  ~2,000 lines the reimplement-then-extract order stands. One consumer cannot
  tell you whether an abstraction is right, and `relay.rs` is under active
  upstream development. The named exit criterion (two consumers, then extract,
  blocking on Wave 3) is unchanged.
- **"Make the fleet screen replace the single-agent transcript in Wave 1."** Not
  quite what was recommended, but worth stating the boundary: the fleet screen is
  *added* and made the default landing surface, and the transcript stays. The
  transcript is what makes the observer/ACP fold worth building and it is the
  only place the "better than the desktop on day one" claim is actually
  demonstrated. R7 names the transcript as the thing that slips if something must.



