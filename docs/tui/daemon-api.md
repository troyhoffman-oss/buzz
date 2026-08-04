# buzz-daemon — API surface for a TUI front

Status: design spec, derived from the crates in `/data/worktrees/buzz-upmerge`
(read-only study, no builds). Shape borrowed from opencode: **one daemon, one
base URL, one event stream, one generated client**.

---

## Part 0 — What the crates actually give us

Everything below is grounded in code that exists today. This section is the
inventory; Part 1 onward is the design.

### 0.1 `buzz-ws-client` — the connection primitive

`crates/buzz-ws-client/src/connection.rs`, `message.rs`, `error.rs` (~560 LOC total).

```rust
pub struct NostrWsConnection { /* ws, buffer: VecDeque<RelayMessage>, pending_challenge, relay_url */ }

NostrWsConnection::connect(url) -> Self
NostrWsConnection::connect_authenticated(url, &Keys, auth_tag: Option<&Tag>) -> Self
  .authenticate(&Keys, Option<&Tag>)           // NIP-42 handshake
  .send_event(Event) -> OkResponse             // EVENT + wait for OK
  .next_event(Duration) -> RelayMessage
  .send_raw(&Value)                            // escape hatch: REQ / CLOSE / COUNT
  .disconnect()

publish_event(relay_url, event, &Keys, auth_tag, timeout_secs) -> OkResponse  // one-shot
```

`RelayMessage` is the full NIP-01 client-facing set: `Event{sub_id, event}`,
`Ok(OkResponse{event_id, accepted, message})`, `Eose{sub_id}`,
`Closed{sub_id, message}`, `Notice{message}`, `Auth{challenge}`,
`Count{sub_id, count}`.

Auth flow, exactly: connect → relay pushes `["AUTH", challenge]` (≤1024 bytes,
enforced) → `build_auth_event(challenge, relay_url, keys, auth_tag)` builds a
kind-22242 NIP-42 event, optionally carrying a NIP-OA `["auth", …]` tag → send
`["AUTH", event]` → wait for `OK` on that event id. Timeouts are constants with
compile-time floors: `AUTH_CHALLENGE_TIMEOUT_SECS = 20`, `AUTH_OK_TIMEOUT_SECS
= 20`, `PUBLISH_OK_TIMEOUT_SECS = 30`.

**What this crate does NOT provide, and the daemon must own:**
- No subscription registry. `send_raw` is the only way to issue `REQ`, and
  nothing tracks sub-ids or re-issues them.
- No reconnect. `connect()` is one-shot; a dropped socket is
  `WsClientError::ConnectionClosed` and that's it.
- No ping/pong keepalive loop (it Pongs reactively inside `recv_one`, but never
  initiates a Ping, so a silent half-open socket is undetected).
- No dedup, no `since` watermarking, no backpressure.

Consequence: `buzz-ws-client` is the right transport primitive for the daemon,
but the daemon reimplements (or lifts from `buzz-acp::relay`) the *session*
layer on top of it.

### 0.2 `buzz-acp/src/relay.rs` — the session layer that already exists

6321 lines. This is the reference implementation of "long-lived authenticated
relay session with subscriptions", and the daemon should be a generalization of
it rather than a fresh design. Public surface:

```rust
HarnessRelay::connect(relay_url, &Keys, agent_pubkey_hex, auth_tag) -> Self
  .discover_channels() -> HashMap<Uuid, ChannelInfo>   // 39002 #p → #d uuids → 39000 metadata
  .rest_client() -> RestClient                          // cheap clone, shares creds
  .subscribe_channel(channel_id, ChannelFilter)         // sub id = "ch-<uuid>"
  .subscribe_channel_from(channel_id, filter, replay_since: Option<u64>)
  .unsubscribe_channel(channel_id)
  .subscribe_membership_notifications()                 // sub id "membership-notif"
  .subscribe_observer_controls()                        // sub id "agent-observer-control"
  .take_observer_control_rx() -> Option<mpsc::Receiver<Event>>
  .next_event() -> Option<BuzzEvent>                    // None = connection lost
  .publish_event(Event) / .try_publish_event(Event)     // await vs fire-and-forget
  .event_publisher() -> RelayEventPublisher             // cloneable publish handle
  .build_typing_event(channel_id, root_id, parent_id) -> Event
  .set_startup_watermark(ts)
  .reconnect() / .shutdown()
```

A background tokio task owns the socket; the foreground talks to it over an
`mpsc<RelayCommand>` (`Subscribe`, `Unsubscribe`, `Reconnect`, `Shutdown`,
`SubscribeMembership`, `SubscribeObserverControls`, `PublishEvent`,
`SetStartupWatermark`). Every hard-won behaviour the TUI daemon needs is
already encoded here as named constants:

| Constant | Value | Why it exists |
|---|---|---|
| `EVENT_CHANNEL_CAPACITY_DEFAULT` | 256 (`BUZZ_ACP_EVENT_BUFFER`) | fan-out backpressure |
| `CMD_CHANNEL_CAPACITY` | 64 | command queue |
| `SEEN_ID_LIMIT` | 12_000 | two-generation dedup set (`TwoGenDedup`) |
| `PING_INTERVAL` / `PONG_TIMEOUT` | 30 s / 10 s | detect half-open sockets |
| `WS_SEND_TIMEOUT_SECS` | 10 | a stalled socket can't wedge the task |
| `STABLE_CONNECTION_SECS` | 60 | resets the backoff ladder after a healthy run |
| `SINCE_SKEW_SECS` | 5 | clock skew tolerance on resubscribe |
| `AUTH_TIMEOUT` / `CONNECT_TIMEOUT` | 20 s / 30 s | tuned for degraded WAN |
| `STARTUP_CONNECT_BACKOFFS` | 1,2,4,8,16 s | shared by initial connect + reconnect |
| `DNS_RETRY_INTERVAL` | 2 s flat (±20% jitter) | DNS brownout ≠ backoff rung |
| `REQ_PACING_INTERVAL` | 125 ms | relay admits ~50 frames / 5 s; a 48-channel resubscribe must not burst |
| `DRAIN_BUDGET_PER_ITER` | 1 | one REQ per main-loop tick |
| `GATED_OBSERVER_QUEUE_CAP` | 256 | observer frames are durable telemetry, parked not dropped, with visible `gated_observer_dropped` accounting |

`BgState` tracks `active_subscriptions: HashMap<Uuid, String>`, `last_seen:
HashMap<Uuid, u64>` (the per-channel `since` watermark), `active_filters`
(replayed on reconnect), plus dropped-event watermarks so backpressure loss is
recovered by replay rather than silently lost.

**The single most important design lesson in this file:** ephemeral events
(typing) are *dropped* under a rate-limit gate; observer telemetry frames are
*parked in order* and paced out. The daemon must preserve that distinction —
the two classes have opposite failure semantics.

### 0.3 `RestClient` — the HTTP bridge composition

Also in `relay.rs` (line ~233):

```rust
pub struct RestClient { http: reqwest::Client, base_url: String, keys: Keys, auth_tag_json: Option<String> }
  .query(&[nostr::Filter]) -> Value      // POST /query
  .count(&[nostr::Filter]) -> Value      // POST /count
  .submit_event(&Event)    -> Value      // POST /events
```

Every request signs a fresh NIP-98 (kind 27235) event with `u`, `method`,
`nonce` (a fresh UUID — this is what makes retries safe against the relay's
replay guard) and `payload` (sha256 of the body), base64s it into
`Authorization: Nostr <b64>`, and attaches `x-auth-tag` when a NIP-OA tag is
configured. Retries: 4 attempts, base delays 500 ms / 1 s / 2 s with ±20%
jitter, on `429 | 502 | 503 | 504` plus connect/timeout errors. NIP-98 is
re-signed per attempt (±60 s window).

### 0.4 `buzz-cli/src/client.rs` — how a headless client composes it

2477 lines. `BuzzClient { http, relay_url, keys, auth_tag, auth_tag_json }`.
This is the closest thing to a spec for "what a non-desktop Buzz client needs".

```rust
BuzzClient::new(relay_url, keys, auth_tag, auth_tag_json)
  .keys() / .relay_url() / .auth_tag_owner_hex()
  .sign_event(EventBuilder) -> Event          // injects NIP-OA tag; ENFORCES exactly-one auth tag
  .sign_event_unchecked(EventBuilder)         // NIP-IA 9035/9036 only
  .query(&filter) / .query_multi(&[filter]) -> String
  .query_paginated(filter, limit) -> Vec<Value>
  .query_all(filter) -> Vec<Value>
  .count(&filter) -> String
  .get_public(path) / .get_authed(path) -> String
  .submit_event(Event) -> String
  .publish_ephemeral_event(Event) -> String   // WS path: ephemerals aren't stored
  .upload_file(path) -> BlobDescriptor
  .download_media(input) -> Bytes
```

Load-bearing details:

- **Pagination is a composite cursor.** `advance_query_cursor` sets
  `filter["until"] = last.created_at` **and** `filter["before_id"] = last.id`.
  Page size `QUERY_PAGE_SIZE`; a short page terminates. A naive `until`-only
  cursor loses or repeats same-second events — the daemon must use both fields.
- **Timeouts are env-tunable**: `BUZZ_TIMEOUT_SECS` (30), `BUZZ_CONNECT_TIMEOUT_SECS` (15).
- **Moderation kinds 9040–9044 have a different retry policy.** They execute at
  the relay *before* dedup, so an ambiguous outcome surfaces as
  `CliError::DeliveryUnknown` rather than being blindly retried. Only a TCP
  connect error or a pre-ingest `429` carrying a `rate-limited:` body is
  safe to retry. The daemon must carry this rule forward verbatim.
- **Exit-code taxonomy** (from the CLI contract): `0` ok, `1` input, `2`
  network/relay, `3` auth, `4` other, `5` write conflict (NIP-33 LWW). This maps
  cleanly onto daemon HTTP status codes (see §3.7).
- **Auth env vars**: `BUZZ_RELAY_URL` (default `http://localhost:3000`),
  `BUZZ_PRIVATE_KEY` (hex or nsec, required), `BUZZ_AUTH_TAG` (NIP-OA tag JSON,
  optional — verified via `buzz_sdk::nip_oa::verify_auth_tag` at load).

### 0.5 Relay HTTP surface (`crates/buzz-relay/src/router.rs`)

The complete non-git, non-admin surface a client can reach:

| Route | Auth | Note |
|---|---|---|
| `GET /` | — | NIP-11 doc, or WebSocket upgrade |
| `GET /info` | — | relay info |
| `GET /.well-known/nostr.json` | — | NIP-05 |
| `GET /health`, `/_liveness`, `/_readiness` | — | probes |
| `POST /events` | NIP-98 | submit any signed event |
| `POST /query` | NIP-98 | array of Nostr filters; NIP-50 `search` routed to Postgres FTS |
| `POST /count` | NIP-98 | NIP-45 |
| `GET/POST /operator/communities[/…]` | NIP-98 | provision/archive/transfer/availability |
| `POST /api/invites`, `GET /api/join-policy`, `POST /api/invites/claim`, `/accept-policy`, `GET /api/join-policy/{terms,privacy}` | mixed | onboarding |
| `GET /moderation/{reports,audit,restricted}` | NIP-98 + mod authz | structured rows, not events |
| `POST /hooks/{id}` | webhook secret | workflow trigger |
| `PUT /upload`, `PUT /media/upload`, Blossom GET | BUD-01 (kind 24242) | media |
| `GET /git/{owner}/{repo}/info/refs`, `git-upload-pack`, `git-receive-pack` | git creds | smart HTTP |
| `GET /huddle/{channel_id}/audio` | WS | audio |

Body cap: 1 MB on the API router.

**The p-gate**: a query without explicit `kinds` is rejected 403. Every daemon
read path must set `kinds`. (`messages search` hits this too — hence the CLI's
`--kinds 9,45001,45003` requirement.)

### 0.6 `buzz-sdk` — the operation vocabulary

`builders.rs` is 4516 lines of `fn(params) -> Result<EventBuilder, SdkError>`.
The crate holds no keys and makes no network calls: `caller params → builder →
validate → EventBuilder → caller signs → Event`. That is exactly the shape the
daemon wants — the daemon is the thing that holds the key and does the network.

Builders grouped by what a TUI would surface:

**Messages / threads**
`build_message`, `build_forum_post`, `build_forum_comment`, `build_diff_message`,
`build_edit`, `build_delete_message`, `build_delete_message_with_options`,
`build_delete_compat`, `build_note` (kind 1), `build_set_canvas`.
`ThreadRef{root_event_id, parent_event_id}` encodes NIP-10 markers: direct
reply emits `["e", root, "", "reply"]`; nested emits `root` + `reply` pairs.

**Reactions / emoji**
`build_reaction`, `build_custom_emoji_reaction`, `build_remove_reaction`,
`build_vote` (`VoteDirection::{Up,Down}`), `build_custom_emoji_set`,
`normalize_custom_emoji_shortcode`. `CustomEmoji{shortcode, url}` per NIP-30.

**Channels**
`build_create_channel`, `build_update_channel`, `build_set_topic`,
`build_set_purpose`, `build_join`, `build_leave`, `build_archive`,
`build_unarchive`, `build_delete_channel`, `build_add_member`,
`build_remove_member`, `extract_channel_id(&Event) -> Option<Uuid>`.

**Identity / presence / social**
`build_profile`, `build_presence_update(status)` (kind 20001),
`build_user_status(text, emoji)` (kind 30315), `build_contact_list`.

**DMs**
`build_dm_open(&[pubkey])`, `build_dm_add_member(channel_id, pubkey)`.

**Agents**
`build_agent_observer_frame(recipient_pubkey, agent_pubkey, frame, encrypted_content)`
— kind 24200, validates `frame ∈ {telemetry, control}` and that content looks
like NIP-44 v2 ciphertext.

**Moderation**
`build_moderation_ban`, `_unban`, `_timeout`, `_untimeout`, `_resolve_report`
(kinds 9040–9044), `build_archive_identity_request` / `_unarchive_` (9035/9036).

**Git / NIP-34** (out of TUI v1 scope, catalogued for completeness)
`build_repo_announcement[_with_tags]`, `build_git_patch`, `build_git_issue`,
`build_git_status`, `build_git_pull_request`, `build_git_pr_update`, with
`GitRepoCoord`, `GitPatchMeta`, `GitIssueMeta`, `GitStatusMeta`,
`GitPullRequestMeta`, `GitPrUpdateMeta`, `GitStatus`, `GitAppliedPatchRef`.

**Workflows / projects**
`build_workflow_def`, `_update`, `_delete`, `_trigger`, `_approval`,
`build_project[_with_tags]`, `validate_project_envelope`, `ProjectMemberCoord`,
`build_delete_addressable`.

`SdkError` variants: `ContentTooLarge{max,got}`, `InvalidTag`, `EmojiTooLong`,
`TooManyMentions` (cap 50), `InvalidDiffMeta`, `InvalidInput`.

`mentions.rs`: `extract_at_names`, `extract_at_mentions_with_known` (multi-word
display names, longest-first), `match_names_to_profiles`, `MentionProfile{pubkey,
content_json}`, `MENTION_CAP`. **This is a pure function over channel-member
profiles** — meaning mention resolution requires the daemon to hold a member
directory cache (§4.2).

`nip_oa.rs`: `compute_auth_tag`, `verify_auth_tag`, `parse_auth_tag`.

### 0.7 Event kinds the TUI cares about

From `crates/buzz-core/src/kind.rs` (authoritative registry, ~120 constants):

| Purpose | Kind(s) |
|---|---|
| Chat message | `40002` (v2), `9` (legacy), edit `40003`, diff `40008` |
| Forum | post `45001`, vote `45002`, comment `45003` |
| System message | `40099`; canvas `40100` |
| Reaction | `7`; deletion `5` |
| Channel metadata / admins / members / roles | `39000` / `39001` / `39002` / `39003` |
| Thread summary | `39005` |
| Profile / contacts | `0` / `3` |
| Presence | `20001` (ephemeral); snapshot `40902`; user status `30315` |
| Typing | `20002` (ephemeral) |
| Read state | `30078` (NIP-78, **NIP-44 self-encrypted**) |
| Mute / pin / bookmark / emoji lists | `10000` / `10001` / `10003` / `10030`, sets `30000`/`30003`/`30030` |
| DM | gift wrap `1059`, open `41010`, add member `41011`, hide `41012`, created `41001`, visibility `30622` |
| Agent observer frame | `24200` (ephemeral, NIP-44) |
| Agent profile / managed agent / team / persona | `10100` / `30177` / `30176` / `30175` |
| Agent turn metric | `44200` (result-gated) |
| Membership notifications | `44100` added / `44101` removed |
| Moderation | `9040`–`9044`, report `1984` |
| Workflow | def `30620`, trigger `46020`, lifecycle `46001`–`46012` |
| Media upload | `49001`; file metadata `1063` |
| Huddle | `48100`–`48106` |

Two access-control sets matter to the daemon:
- `AUTHOR_ONLY_KINDS = [30300 (reminder), 30350 (push lease)]` — relay never
  reveals existence to anyone but the author.
- `RESULT_GATED_KINDS = [30622 (DM visibility), 44200 (agent turn metric)]` —
  even an id-only read requires a matching `#p` tag.

### 0.8 The observer decrypt path — what key material the daemon needs

This is the crux for the activity viewer. `crates/buzz-core/src/observer.rs`:

```rust
pub const OBSERVER_AGENT_TAG: &str = "agent";
pub const OBSERVER_FRAME_TAG: &str = "frame";
pub const OBSERVER_FRAME_TELEMETRY: &str = "telemetry";   // agent → owner
pub const OBSERVER_FRAME_CONTROL:   &str = "control";     // owner → agent
pub const NIP44_MIN_CONTENT_LEN: usize = 132;
pub const NIP44_MAX_CONTENT_LEN: usize = 87_472;
pub const OBSERVER_MAX_PLAINTEXT_LEN: usize = 65_535;

content_looks_like_nip44(&str) -> bool
encrypt_observer_payload<T: Serialize>(sender: &Keys, recipient: &PublicKey, &T) -> String
decrypt_observer_payload<T: DeserializeOwned>(recipient: &Keys, event: &Event) -> T
```

`decrypt_observer_payload` calls `nip44::decrypt(recipient_keys.secret_key(),
&event.pubkey, &event.content)`. NIP-44 conversation keys are ECDH — symmetric
in the pair `(sender_pubkey, recipient_secret)`. So:

> **Key material the daemon needs: the owner's secret key, and nothing else.**
> No per-agent key, no key exchange, no derived material to cache. The frame's
> `event.pubkey` (the agent) is the other half of the ECDH, and it travels on
> the event.

Frame anatomy (`build_agent_observer_frame`, kind 24200):
- content: NIP-44 v2 ciphertext (length-validated at build time)
- `["p", recipient_pubkey]` — cleartext, this is what the relay routes on
- `["agent", agent_pubkey]` — which agent's stream this belongs to
- `["frame", "telemetry"|"control"]` — direction, cleartext

Subscription filter for the viewer (from `desktop/src/shared/api/observerRelay.ts`,
the reference consumer):

```json
{ "kinds": [24200], "#p": ["<owner_pubkey>"],
  "limit": 1000, "since": now - OBSERVER_LIVE_LOOKBACK_SECS }
```

High `limit` so reconnect replay recovers missed frames; `since` lookback so
session/prompt frames emitted just before subscribe aren't lost; dedup on
`(seq, timestamp)` prevents double-processing.

The desktop's Rust command (`desktop/src-tauri/src/commands/identity.rs
::decrypt_observer_event`) adds **two defense-in-depth checks the daemon must
replicate**:

```rust
if !event.verify_id() { return Err("observer event has invalid ID"); }
if !event.verify_signature() { return Err("observer event has invalid signature"); }
buzz_core::observer::decrypt_observer_payload(&keys, &event)
```

And the store layer (`observerRelayStore.ts`) adds a third, at the application
level: the event's `pubkey` must equal its own `agent` tag value, and that
pubkey must be a *known/registered* agent. A frame that decrypts but whose
sender doesn't match its claimed agent tag is discarded. All three guards
belong in the daemon, not the TUI.

Decrypted payload shape (`ObserverEvent` in `buzz-acp/src/observer.rs`,
serialized camelCase):

```rust
pub struct ObserverEvent {
    pub seq: u64,                     // monotonic, process-local to the agent
    pub timestamp: String,            // RFC3339 UTC
    pub kind: String,                 // e.g. "acp_read", "turn_started"
    pub agent_index: Option<usize>,   // pool slot
    pub channel_id: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub started_at: Option<String>,   // authoritative turn start
    pub payload: serde_json::Value,   // raw or semantic ACP payload
}
```

`OBSERVER_BUFFER_CAP = 1_000` on the agent's side replay buffer.

The control direction is symmetric: `build_observer_control_event` encrypts a
payload to the **agent** pubkey and tags `p = agent`, `agent = agent`,
`frame = control`. The daemon needs the same helper to send interrupts /
permission responses from the TUI.

---

## Part 1 — The daemon boundary

### 1.1 The opencode shape, stated plainly

opencode's client/server split has four properties worth stealing wholesale:

1. **Single URL coupling.** The client knows exactly one thing about the
   server: a base URL. Everything else — endpoints, event stream, capabilities
   — hangs off it. No second port, no side-channel, no "also connect to the
   relay directly for X".
2. **One event stream.** A single SSE endpoint carries every push. The client
   does not maintain N subscriptions; it maintains one connection and
   demultiplexes by event type.
3. **A written spec, generated clients.** The server publishes OpenAPI; clients
   are generated. The TUI never hand-rolls request structs.
4. **Auto-spawn with attach.** The client starts the server if it isn't
   running, and a second client attaches to the same one. The server outlives
   any single client.

Mapped onto Buzz:

```
┌─ TUI ────────┐   HTTP + SSE over UDS      ┌─ buzz-daemon ──────────────┐   WS (NIP-42) ┌───────┐
│  ratatui     │ ─────────────────────────► │  session, cache, decrypt   │ ────────────► │ relay │
│  no crypto   │ ◄───────────────────────── │  key custody               │ ◄──────────── │       │
│  no relay    │   /event (ndjson/SSE)      │                            │   HTTP bridge └───────┘
└──────────────┘                            │  provider passthrough      │ ──► buzz-backend-ssh
                                            └────────────────────────────┘     buzz-backend-kubernetes
```

The TUI holds **no secret key, no relay URL, no NIP-44 code, no Nostr
dependency at all**. It speaks JSON to a local socket. That is the whole point:
it makes the TUI a thin renderer, and it makes a second front-end (a web UI, a
`buzz tui --attach`, a scriptable client) free.

### 1.2 Why a daemon at all, rather than the TUI opening the relay socket

Four reasons, each grounded in something above:

- **Reconnect and subscription state are expensive and stateful.** §0.2 lists
  thirteen tuned constants and a `BgState` with five recovery watermarks. That
  logic should exist once, in a process that outlives a `Ctrl-C`.
- **The cache must survive the TUI.** Restarting a TUI must not re-page 500
  channels and 10k profiles out of the relay.
- **Key custody is a boundary.** The secret key touches NIP-42 auth, NIP-98
  signing, event signing, NIP-44 observer decrypt, and read-state
  self-encryption. Keeping it in one process with one audited surface is
  strictly better than spreading it into a UI process.
- **Second-client attach.** A `buzz tui` in one pane and a `buzz watch` in
  another should share one relay connection, one auth session, one rate-limit
  budget. The relay admits ~50 frames / 5 s per connection (§0.2
  `REQ_PACING_INTERVAL`); N independent clients would each burn that budget.

---

## Part 2 — Lifecycle

### 2.1 Transport: Unix domain socket, HTTP/1.1 over it

```
$XDG_RUNTIME_DIR/buzz/<community-hash>.sock      # Linux
~/Library/Application Support/buzz/run/<hash>.sock  # macOS
```

`<community-hash>` = first 16 hex chars of `sha256(relay_url + ":" + pubkey)`.
One daemon per (relay, identity) pair. Two communities → two daemons; that is
correct, because the desktop app already treats a relay boundary as a hard
remount boundary (see the project's `resetCommunityState()` discipline — every
module-level cache is community-scoped and must be reset on relay change; a
daemon per relay makes that structural instead of a checklist).

UDS, not TCP, because: no port allocation, no port collisions, filesystem
permissions (`0600`) are the authorization model, and nothing is reachable off
the box. A `--listen 127.0.0.1:PORT` flag exists for the containerized case and
is opt-in only.

### 2.2 Auto-spawn handshake

```
TUI start
  ├─ resolve socket path from (BUZZ_RELAY_URL, identity)
  ├─ connect(socket)
  │    ├─ success → GET /health → version check → attach
  │    └─ ENOENT / ECONNREFUSED
  │         ├─ acquire flock on <hash>.lock          (races between two TUIs)
  │         ├─ stale socket? unlink it
  │         ├─ spawn `buzz-daemon --socket <path> --detach`
  │         ├─ poll connect() every 25 ms, ≤ 5 s
  │         └─ release flock
  └─ GET /session → identity, relay, connection state
```

The daemon writes its pid + version + start time to `<hash>.json` next to the
socket. A version mismatch on attach is a hard error with a clear message
(`daemon 0.4.1 is running; this client needs ≥0.5.0 — run 'buzz daemon
restart'`), never a silent protocol negotiation.

### 2.3 Daemon lifetime

- **Idle shutdown**: no client connected for `--idle-timeout` (default 30 min)
  → graceful shutdown. `--idle-timeout 0` disables.
- **Explicit**: `POST /daemon/shutdown`, or `buzz daemon stop`.
- **Never on last-client-disconnect.** An agent turn may be streaming observer
  frames the user wants to see when they come back; the daemon keeps
  subscribing and buffering.
- **Crash**: the TUI's SSE stream errors → shows "daemon lost, reconnecting" →
  re-runs the auto-spawn handshake. Cache is rebuilt from the local store
  (§4.4), not from the relay.

### 2.4 Second-client attach

Multiple clients, one daemon. Each `GET /event` connection is an independent
subscriber to an internal `tokio::sync::broadcast`. Semantics:

- **Event stream is broadcast.** Every connected client sees every event it is
  subscribed to. No client can starve another.
- **Per-client cursor.** `GET /event?since=<cursor>` replays from the daemon's
  ring buffer (bounded, see §4.4) so a reattaching TUI catches up rather than
  showing a gap.
- **Writes are unsynchronized.** Two clients posting messages is fine — they
  are separate events. The daemon does not lock.
- **Read-state is last-writer-wins** on the relay already (kind 30078 is NIP-33
  replaceable); the daemon serializes its own publishes so two clients don't
  produce interleaved partial blobs.
- **Slow-consumer policy**: a client whose SSE write buffer backs up past N
  events is disconnected with a `stream.overflow` frame, not silently lagged.
  Mirrors the `BgState` philosophy — loss is always accounted for, never
  silent.

---

## Part 3 — The API surface

Conventions:
- Base: `http://localhost/` over the UDS (Host header ignored).
- All bodies JSON. All responses `{...}` objects, never bare arrays (so fields
  can be added without breaking clients).
- `GET` = cached read, may be served entirely from daemon state.
- `POST` = mutation, always produces a relay write.
- OpenAPI 3.1 document served at `GET /openapi.json`; the TUI's client is
  generated from it. Adding an endpoint without adding it to the spec is a
  build failure.

### 3.0 Meta

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | `{status, version, uptime_secs, relay_state}` — the attach probe |
| `GET` | `/openapi.json` | the spec |
| `GET` | `/daemon` | pid, version, socket, connected clients, cache stats, subscription count |
| `POST` | `/daemon/shutdown` | graceful stop |
| `POST` | `/daemon/reconnect` | force relay reconnect (maps to `RelayCommand::Reconnect`) |

### 3.1 Auth / session

The daemon owns the key. The TUI never sees it.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/session` | `{pubkey, npub, relay_url, community: {name, icon}, connection: {state, since, attempts}, auth_tag_owner?}` |
| `POST` | `/session/identity` | load an identity: `{nsec}` or `{keyfile}` or `{keychain: true}`. Returns pubkey. Refuses if a session is already live unless `force`. |
| `DELETE` | `/session/identity` | drop key from memory, disconnect, keep daemon alive |
| `GET` | `/session/relay-info` | NIP-11 doc, cached (proxies `GET /info`) |
| `POST` | `/session/join` | invite claim → proxies `POST /api/invites/claim`; also `GET /session/join-policy` for the pre-claim policy fetch |

Connection state enum, surfaced verbatim to the TUI status bar:
`disconnected | connecting | authenticating | connected | rate_limited |
reconnecting{attempt, next_retry_in_ms} | dns_brownout | auth_failed{reason}`.
Every one of these corresponds to a real state in `buzz-acp/src/relay.rs`;
`rate_limited` and `dns_brownout` in particular are states the desktop learned
to surface and a TUI must too, because they look identical to "hung" otherwise.

### 3.2 Channels

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/channel` | list. Query: `visibility`, `member=true`, `archived`, `limit`. Served from cache. |
| `GET` | `/channel/{id}` | one channel: metadata (39000), member count, topic, purpose, unread summary |
| `POST` | `/channel` | create (`build_create_channel`) |
| `PATCH` | `/channel/{id}` | update / topic / purpose (`build_update_channel`, `build_set_topic`, `build_set_purpose`) |
| `POST` | `/channel/{id}/join` | `build_join` |
| `POST` | `/channel/{id}/leave` | `build_leave` |
| `POST` | `/channel/{id}/archive` \| `/unarchive` | `build_archive` / `build_unarchive` |
| `DELETE` | `/channel/{id}` | `build_delete_channel` |
| `GET` | `/channel/{id}/member` | members (39002), joined with profiles |
| `POST` | `/channel/{id}/member` | `build_add_member` |
| `DELETE` | `/channel/{id}/member/{pubkey}` | `build_remove_member` |
| `GET`/`PUT` | `/channel/{id}/canvas` | 40100 (`build_set_canvas`) |

Discovery mirrors `HarnessRelay::discover_channels()`: query 39002 with
`#p=<self>` → collect `#d` UUIDs → batch-query 39000 for metadata → drop
archived. The daemon does this once at connect and then maintains it live from
the membership subscription (44100/44101).

### 3.3 Messages / threads

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/channel/{id}/message` | history. Query: `limit`, `before` (composite cursor, see below), `since`, `kinds` |
| `POST` | `/channel/{id}/message` | send. Body: `{content, reply_to?, kind?, mentions?[], files?[], broadcast?}` |
| `GET` | `/message/{event_id}` | single event, hydrated |
| `GET` | `/message/{event_id}/thread` | full thread. Query: `limit`, `depth_limit` |
| `PATCH` | `/message/{event_id}` | edit (`build_edit`, kind 40003) |
| `DELETE` | `/message/{event_id}` | delete (`build_delete_message_with_options`; optional `action_id`, `reason_code`, `public_reason` for moderation tombstones) |
| `POST` | `/channel/{id}/diff` | `build_diff_message` (kind 40008) with `DiffMeta` |
| `POST` | `/channel/{id}/post` | forum post 45001 (`build_forum_post`) |
| `POST` | `/message/{event_id}/comment` | forum comment 45003 |
| `POST` | `/message/{event_id}/vote` | `{direction: "up"|"down"}` → 45002 |

**Pagination contract, exposed honestly.** `before` is an opaque cursor string,
not a timestamp. The daemon encodes `(until, before_id)` into it, matching
`advance_query_cursor` in `client.rs`. Response:

```json
{ "messages": [ … ], "next": "<cursor>|null", "has_more": true }
```

Exposing a bare `until` would reintroduce the same-second duplicate/skip bug the
composite cursor exists to fix.

**Send is optimistic-friendly.** `POST` returns
`{event_id, accepted, message, local_id}` immediately after the relay `OK`. For
the optimistic-echo pattern the TUI wants, the client passes a `local_id` and
the daemon echoes it on both the response and the resulting `message.new`
stream event, so the TUI can reconcile its pending bubble without a content
match.

**Mentions.** `mentions` may contain pubkeys (hex/npub) *or* be omitted, in
which case the daemon runs `buzz_sdk::mentions::extract_at_mentions_with_known`
against its cached member directory for that channel and resolves them. This is
the single strongest argument for the member-directory cache: mention
resolution is a pure function that needs the whole channel roster, and doing it
in the TUI would mean shipping the roster to the TUI.

### 3.4 Search

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/search` | Query: `q`, `author`, `channel`, `since`, `kinds`, `limit` |
| `GET` | `/search/user` | directory search by name → profiles |

Rules baked in, so the TUI cannot get them wrong:
- `kinds` **always** set. Default `[9, 40002, 45001, 45003]`, exactly as
  `cmd_search` does. An unscoped query hits the relay p-gate and 403s.
- `author` accepts hex, npub, **or a display name**; the daemon resolves names
  via a NIP-50 kind-0 search and returns `409 ambiguous_author` with the
  candidate list when a name matches more than one identity — never a silent
  mix of authors.
- Relevance order for `q` queries; newest-first when only `author`/`since` were
  given (no relevance signal exists).
- `limit` clamped to 100.

### 3.5 Presence, typing, read-state

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/presence` | `?pubkeys=a,b,c` — cached presence map |
| `PUT` | `/presence` | `{status: "online"|"away"|"offline"}` → 20001 |
| `PUT` | `/status` | NIP-38 30315 (`build_user_status`); `{clear:true}` removes |
| `POST` | `/channel/{id}/typing` | fire-and-forget 20002 (`build_typing_event`) |
| `GET` | `/read-state` | decrypted read markers |
| `PUT` | `/read-state` | `{contexts: {"<channel>": ts, "msg:<id>": ts, "thread:<id>": ts}}` |
| `POST` | `/channel/{id}/read` | convenience: mark channel read to `ts` (default now) |

Read-state is where the daemon earns the most. Kind 30078 is a NIP-44
**self-encrypted** blob (`ReadStateBlob {v:1, client_id, contexts}`) with real
constraints from `readStateFormat.ts`: 32 KB plaintext budget, up to 8 slots
(each a separate 30078 event with its own `read-state:<slot>` d-tag), 10k
context cap, a 7-day horizon, `msg:` and `thread:` key prefixes with distinct
semantics (a `msg:` marker is grow-only per reply id so reading an ancestor
never covers a descendant), and `maxReadAt` merge across markers.

Slot-splitting, horizon pruning, encryption, and the LWW publish serialization
all live in the daemon. The TUI sends `{"contexts": {...}}` and reads back a
merged map. **None of that logic is reimplementable correctly in a TUI**, and
two clients doing it independently would clobber each other.

Typing indicators use the ephemeral publish path (`try_publish_event` /
fire-and-forget) and are **dropped, not queued**, when the relay rate-limit gate
is armed. `POST /channel/{id}/typing` therefore returns `202` always and never
an error the TUI must handle.

### 3.6 Directory / profiles / mentions

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/user/{pubkey}` | profile (kind 0), presence, status, agent flag |
| `GET` | `/user` | `?pubkeys=…` batch, or `?name=…` search, or `?channel=…` roster |
| `PATCH` | `/me` | `build_profile` — name, avatar, about, nip05 |
| `GET` | `/mention/candidates` | `?channel=<id>&prefix=<str>` — autocomplete source |
| `GET` | `/mention/inbox` | mentions of me. `?since=&limit=` — the "@ mentions" view |

`/mention/candidates` exists because the TUI's `@` autocomplete must be
instantaneous and must match the same longest-first, word-boundary,
case-insensitive algorithm the send path will use. Returning the candidate list
from the same cache the resolver uses guarantees "what you picked is what gets
tagged".

### 3.7 Reactions & emoji

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/message/{event_id}/reaction` | grouped `{emoji, count, reactors[], mine: bool}` |
| `POST` | `/message/{event_id}/reaction` | `{emoji}` or `{shortcode, url}` for NIP-30 custom |
| `DELETE` | `/message/{event_id}/reaction/{emoji}` | `build_remove_reaction` (needs the reaction event id — daemon looks it up from cache) |
| `GET` | `/emoji` | workspace palette = union of all members' 30030 sets |
| `PUT` | `/emoji` | my custom set (`build_custom_emoji_set`) |

The workspace palette is a **computed view, not stored state** (each member
publishes their own 30030; the union is a read-time merge). The daemon computes
and caches it; the TUI just gets a list.

`DELETE …/reaction/{emoji}` is a good example of daemon value: NIP-25 removal
requires the *event id of your own reaction*, which the TUI would otherwise
have to track. The daemon has it in the reaction index.

### 3.8 DMs

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/dm` | conversation list (41010/41001, visibility 30622) |
| `POST` | `/dm` | open a DM with `{pubkeys: []}` (`build_dm_open`) |
| `POST` | `/dm/{id}/member` | `build_dm_add_member` |
| `POST` | `/dm/{id}/hide` | 41012 |

DM message send/read reuses `/channel/{id}/message` — a DM is a channel with a
different type. Gift-wrap (1059) unwrapping happens in the daemon; the TUI sees
plaintext messages like any other channel.

### 3.9 Agent activity stream — the observer viewer

This is the piece the TUI exists for.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/agent` | managed agents (30177) + agent profiles (10100): name, pubkey, runtime, model, backend, state |
| `GET` | `/agent/{pubkey}` | one agent, with current session/turn |
| `GET` | `/agent/{pubkey}/activity` | **decrypted** observer frames, paged. `?channel=&since=&limit=` |
| `GET` | `/agent/{pubkey}/transcript` | activity folded into a transcript (tool start/update pairing, plan replacement, permission req/resp) |
| `POST` | `/agent/{pubkey}/control` | send an encrypted control frame: interrupt, permission response, config change |
| `GET` | `/agent/{pubkey}/metric` | 44200 turn metrics (tokens, duration, cost) |

**The daemon subscribes once**, with the reference filter:

```json
{"kinds":[24200], "#p":["<owner_pubkey>"], "limit":1000, "since": now - LOOKBACK}
```

and for each frame runs the full guard chain before anything reaches a client:

1. `event.verify_id()` — reject on failure
2. `event.verify_signature()` — reject on failure
3. `event.pubkey == event.tags["agent"]` — the sender must be the agent it
   claims to be
4. that pubkey must be a **known/registered** agent (from the 30177/10100 cache)
5. `content_looks_like_nip44(&content)` (132..=87_472 bytes)
6. `nip44::decrypt(owner_secret, event.pubkey, content)`
7. plaintext ≤ 65_535 bytes
8. deserialize to `ObserverEvent`; dedup on `(agent_pubkey, seq, timestamp)`

A frame failing any check is dropped and counted, not surfaced. **Key material
required: the owner secret key only** — NIP-44's ECDH is symmetric over the
pair, and the agent half arrives on the event. There is no key registry, no
per-agent secret, nothing to provision. This is the single most important
finding for the activity viewer: the daemon needs exactly the key it already
has to run NIP-42 and NIP-98.

Control frames are the mirror: encrypt to the *agent's* pubkey, tag
`p=agent, agent=agent, frame=control`, publish. `POST /agent/{pubkey}/control`
takes an opaque `{payload}` and does the encryption + build + sign + publish.

Because 24200 is ephemeral (not stored by the relay), history comes from the
daemon's own local archive (§4.4). `GET /agent/{pubkey}/activity` merges the
live window with archived pages and returns one sorted, deduplicated sequence —
the same discipline the desktop uses (one `buildTranscriptState()` over the
combined set, never two independent state machines whose stateful relationships
split across a boundary).

### 3.10 Deploy passthrough

Backend providers (`buzz-backend-ssh`, `buzz-backend-kubernetes`) are **separate
binaries on PATH**, invoked one-process-per-op: write one JSON request to
stdin, read one JSON response from stdout, always exit 0 (a non-zero exit makes
the caller discard stdout and report raw stderr, throwing away the structured
error). Ops: `info`, `check`, `discover_harnesses`, `probe_models`, `deploy`.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/backend` | discovered providers with their `info` responses (name, version, `protocol_version`, `config_schema`) |
| `POST` | `/backend/{name}/check` | connectivity probe |
| `POST` | `/backend/{name}/discover-harnesses` | what agent CLIs exist on the target |
| `POST` | `/backend/{name}/probe-models` | model availability |
| `POST` | `/backend/{name}/deploy` | deploy a managed agent |
| `GET` | `/backend/job/{id}` | poll a long-running op (also streamed as `backend.progress`) |

The daemon is a **transparent conduit** here, with exactly four
responsibilities and no more:

1. **Provider discovery** on PATH, staging a copy and calling `info` before any
   secret-bearing request is sent, and rejecting a provider with an absent or
   unsupported `protocol_version` (absence is an error, not a presumed `1`).
2. **Secret injection.** `deploy` requires a desktop/daemon-minted
   `private_key_nsec` and fails closed without one. The daemon mints the agent
   identity and injects it; the TUI never sees an nsec.
3. **Redaction.** `protocol::redact` scrubs `nsec1`/`sprt_tok_` prefixed tokens
   from provider stderr; the daemon scrubs again on the way in, so a leak
   requires two independent failures. Keep both layers.
4. **Config validation.** The provider config schema deliberately avoids keys
   whose word-split contains `secret`/`password`/`token`/`key`/`credential` —
   the validator drops them silently. This is why the field is
   `ssh_identity_file` and not `ssh_key_path`. The daemon must apply the same
   validator, and surface a *loud* error rather than a silent drop.

The `config_schema` from `info` is passed through untouched so the TUI can
render a form from it — including the Tailscale-decorated `oneOf` on `ssh_host`
when a tailnet is detected, which degrades structurally (no `oneOf` → plain
text field) rather than behind a feature flag. `recovery: {action:"open_url",
url}` on a failure is likewise passed through so the TUI can offer a browser
re-auth.

### 3.11 Media

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/media` | upload (multipart or path) → `BlobDescriptor{url, sha256, size, type}` |
| `GET` | `/media/{sha256}` | download through the daemon's cache |

Uploads use BUD-01 kind-24242 Blossom auth. The daemon caches blobs on disk
keyed by sha256 so a re-render doesn't re-fetch.

### 3.12 Moderation (thin)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/moderation/report` \| `/audit` \| `/restricted` | proxies the relay's structured-row endpoints |
| `POST` | `/moderation/ban` \| `/unban` \| `/timeout` \| `/untimeout` \| `/resolve-report` | 9040–9044 |

**These carry the non-idempotent retry policy.** Kinds 9040–9044 execute at the
relay before dedup, so a blind resend can duplicate the mutation. Ambiguous
outcomes return `409 delivery_unknown` with the event id, and the TUI must show
"unknown — check the audit log" rather than auto-retrying. Only a TCP connect
error or a pre-ingest `429` with a `rate-limited:` body is retried internally.

### 3.13 Error model

One shape everywhere:

```json
{ "error": { "code": "rate_limited", "message": "…", "retry_after_ms": 4200, "detail": {…} } }
```

`code` is a stable string; HTTP status maps onto the CLI's exit-code taxonomy:

| CLI exit | Meaning | Daemon status | `code` examples |
|---|---|---|---|
| 1 | input error | `400` | `invalid_filter`, `content_too_large`, `too_many_mentions` |
| 2 | network / relay | `502` / `503` | `relay_unreachable`, `rate_limited` |
| 3 | auth | `401` / `403` | `not_authenticated`, `p_gate`, `not_a_member` |
| 4 | other | `500` | `internal` |
| 5 | write conflict (NIP-33 LWW) | `409` | `write_conflict` |
| — | ambiguous moderation write | `409` | `delivery_unknown` |
| — | ambiguous author name | `409` | `ambiguous_author` |

`SdkError` variants map straight through: `ContentTooLarge{max,got}`,
`EmojiTooLong`, `TooManyMentions`, `InvalidDiffMeta`, `InvalidInput`,
`InvalidTag`.

---

## Part 4 — The event stream

### 4.1 One endpoint

```
GET /event
GET /event?since=<cursor>&channel=<id>&topic=agent,message
Accept: text/event-stream        → SSE
Accept: application/x-ndjson     → newline-delimited JSON
```

ndjson is the default for the TUI (simpler to parse, no `data:` framing, no
reconnect semantics baked into the transport that we'd have to fight); SSE is
offered because browsers and `curl` want it and it costs one content-negotiation
branch.

**Every push goes through this one stream.** There is no second socket, no
per-channel connection, no long-poll fallback. This is the property that makes
the client thin: the TUI's entire network layer is one HTTP client plus one
line-reader.

### 4.2 Frame shape

```json
{ "seq": 10231,
  "ts": "2026-08-04T18:22:41.113Z",
  "type": "message.new",
  "channel_id": "…",
  "data": { … } }
```

`seq` is daemon-global and monotonic — it is the reconnect cursor. `type` is a
dotted namespace so a client can subscribe coarsely (`topic=agent`) or
demultiplex finely.

### 4.3 Event catalogue

**Connection / session**
| type | data |
|---|---|
| `connection.state` | `{state, attempt?, next_retry_in_ms?, reason?}` |
| `session.identity` | identity loaded/cleared |
| `stream.overflow` | `{dropped, since_seq}` — slow consumer, before disconnect |

**Messages**
| type | data |
|---|---|
| `message.new` | hydrated message + `local_id?` for optimistic reconcile |
| `message.edit` | 40003 |
| `message.delete` | tombstone, incl. public reason fields |
| `message.reaction` | `{event_id, emoji, pubkey, added: bool}` |
| `thread.reply` | reply landed under a root you're viewing (carries updated `reply_count`, `descendant_count`) |

**Channels**
| type | data |
|---|---|
| `channel.new` / `channel.update` / `channel.archive` | 39000 changes |
| `channel.member` | `{channel_id, pubkey, added: bool}` (39002 / 44100 / 44101) |
| `channel.unread` | recomputed unread + mention counts |

**Presence / typing**
| type | data |
|---|---|
| `presence.update` | 20001 coalesced (see §5.3) |
| `typing.start` | `{channel_id, pubkey, thread?}` — daemon synthesizes `typing.stop` on a timer; the wire has no stop event |
| `status.update` | 30315 |

**Agent activity — the reason this stream exists**
| type | data |
|---|---|
| `agent.frame` | one decrypted `ObserverEvent` |
| `agent.turn.start` / `agent.turn.end` | derived from frames, with `turn_id` + authoritative `started_at` |
| `agent.permission.request` | needs a TUI answer → `POST /agent/{pk}/control` |
| `agent.state` | idle / working / errored, per agent |
| `agent.metric` | 44200 |

**Backend**
| type | data |
|---|---|
| `backend.progress` | `{job_id, phase, line}` — deploy output streamed live |
| `backend.done` | `{job_id, ok, result?, error?, recovery?}` |

**Read state / mentions**
| type | data |
|---|---|
| `read_state.update` | another device moved a marker |
| `mention.new` | you were mentioned |

### 4.4 Reconnect and the ring buffer

The daemon keeps a bounded ring of recent stream frames (default 10k, matching
the order of magnitude of `SEEN_ID_LIMIT`'s 12k dedup window). `?since=<seq>`
replays from it. If the cursor has fallen out of the ring, the daemon responds
with a `stream.reset` frame first, telling the TUI to invalidate and re-fetch
rather than silently presenting a gapped timeline.

Same principle as `gated_observer_dropped` and `membership_dropped_since` in the
harness: **loss is always visible**.

---

## Part 5 — What the daemon caches vs passes through

### 5.1 Cached (authoritative in the daemon, served without touching the relay)

| Data | Kinds | Why |
|---|---|---|
| Channel list + metadata | 39000, 39002 | Read on every render; changes rarely; discovery is a 2-round-trip join |
| Member rosters | 39002 | Mention resolution needs the whole roster |
| Profiles | 0 | Every message renders an author; N-per-screen lookups are unacceptable |
| Message windows (recent N per channel) | 9/40002/45001/45003 | Scrollback and reconnect gap-fill |
| Reaction index | 7 | Grouping + "which reaction event is mine" for removal |
| Read state (decrypted) | 30078 | Slot merge, horizon prune, LWW serialization |
| Presence | 20001, 40902 | Ephemeral: only the daemon's live view exists |
| Observer frames (decrypted) | 24200 | **Ephemeral — the relay does not store them.** If the daemon doesn't archive, the history is gone |
| Emoji palette (union) | 30030, 10030 | Computed view over all members |
| Agent registry | 30177, 10100 | Needed for the observer sender-identity guard |
| Blossom blobs | — | Content-addressed, immutable |

Backing store: SQLite at `~/.local/share/buzz/<community-hash>/cache.db`, so a
daemon restart doesn't re-page the world. Observer frames are stored
**decrypted** — which means the file needs `0600` and the same disposition as a
key file. (Alternative: store ciphertext and decrypt on read. Slower, but
strictly better at rest. Decide before shipping; the design supports either
because decrypt is a pure function of the owner key plus the event.)

### 5.2 Passed through (no caching, straight to the relay)

- **Search.** Results are relevance-ranked by Postgres FTS and change with the
  corpus. Caching them produces confidently stale answers.
- **Moderation reads** (`/moderation/*`). Structured rows with their own authz
  gate; a stale ban list is a safety problem.
- **Media upload.** Streamed.
- **Git smart-HTTP.** Not the daemon's business at all (§6).
- **Backend provider ops.** Stateless per-op subprocesses by construction.
- **Operator/community provisioning.** Rare, consequential, must be live.
- **`POST /count`.** Cheap at the relay, meaningless when stale.

### 5.3 Coalesced (cached but rate-limited outward)

- **Presence.** 20001 is ephemeral and chatty. The daemon maintains the live map
  and emits `presence.update` at most every 2 s, batched.
- **Typing.** Emit `typing.start` on first frame, suppress repeats, synthesize
  expiry after ~5 s of silence. The wire protocol has no stop event, so the
  daemon must invent it — doing this in each client would produce three
  different flicker behaviours.
- **Unread counts.** Recomputed on message arrival and read-state change, but
  emitted at most every 500 ms.

---

## Part 6 — What stays OUT of the daemon

Explicit non-goals. Each is a thing that would be tempting and would be wrong.

1. **Rendering, layout, key bindings, themes.** The daemon returns data. It has
   no opinion about markdown rendering, code-block highlighting, or column
   widths. (Corollary: it returns message `content` raw, plus parsed structure
   where the *protocol* defines it — tags, imeta, mentions — never HTML.)

2. **Agent process supervision.** The daemon does not spawn or babysit
   `buzz-acp` / agent CLIs. Providers own that (systemd --user on the SSH
   backend, a controller on Kubernetes). The daemon is an observer and a
   deploy-request conduit. Blurring this would recreate the desktop's managed-
   agent supervisor inside a chat client.

3. **The relay itself.** No embedded relay, no local event store pretending to
   be one, no offline write queue that replays into the relay later. Writes go
   to the relay or they fail visibly. Offline-write reconciliation against
   NIP-33 LWW replaceables is a distributed-systems project, not a TUI feature.

4. **Git operations.** `git` is a better git client than we will write. The
   relay speaks smart HTTP with `git-credential-nostr`; the TUI shells out.
   NIP-34 patch/PR/issue *events* are ordinary events and go through the normal
   read paths — but no packfile ever touches the daemon.

5. **Huddle audio.** `GET /huddle/{id}/audio` is a separate WS with real-time
   constraints. Voice belongs in a process that can be killed without dropping
   the chat session.

6. **Community provisioning / operator surface.** `/operator/*` is
   administrative, rare, and better served by `buzz-admin`. The daemon proxies
   invite *claim* (onboarding) and nothing else.

7. **Workflow authoring.** `buzz-workflow` YAML with evalexpr conditions is an
   editor problem. The daemon surfaces workflow *events* (46001–46012) and
   trigger/approval actions; it does not become a workflow IDE.

8. **Multi-community aggregation.** One daemon = one (relay, identity). A
   unified inbox across communities is a *client* concern: the TUI opens two
   daemons and merges. Putting multi-tenancy inside the daemon would reintroduce
   exactly the cross-community cache-leak class that the desktop's
   `resetCommunityState()` discipline exists to prevent — except without the
   React remount boundary that makes it tractable there.

9. **A second auth model.** No API tokens, no daemon-level users, no bearer
   auth. Filesystem permissions on the UDS are the authorization boundary. If
   you can open the socket, you are the user.

10. **Business logic the relay owns.** Permission checks, membership gates, the
    p-gate, rate limits, moderation authz — the daemon reflects the relay's
    answers and never second-guesses them with a local approximation that can
    drift.

---

## Part 7 — Open questions

1. **Observer frames at rest: plaintext or ciphertext?** Plaintext is fast and
   simple; ciphertext is strictly better if the disk is compromised. Decrypt is
   a pure function of (owner key, event), so either works — but this must be
   decided before the archive schema is written, not after.

2. **Does the daemon reuse `buzz-acp::relay`'s background task, or generalize
   it?** The logic is right but the shape is agent-shaped (`agent_pubkey_hex`
   threaded through, `ChannelFilter{kinds, require_mention}`, harness-specific
   commands). Cleanest answer: extract the session layer into a new
   `buzz-session` crate that both `buzz-acp` and `buzz-daemon` depend on, rather
   than forking 6300 lines. That is a real refactor with real merge risk against
   an actively-developed file — worth scoping before committing.

3. **Cursor opacity.** Opaque cursors are the right API, but debugging is much
   easier when they're readable. Suggest base64url of `{"until":…,"before_id":…}`
   — opaque enough that nobody parses it, transparent enough to decode by hand.

4. **Backpressure on `/event` when the TUI is in a modal.** Disconnect-on-
   overflow is honest but harsh. An alternative is per-topic drop policies
   (drop `presence.update`, never drop `agent.frame`) — which mirrors the
   ephemeral-vs-durable distinction the harness already makes. Probably the
   right answer; needs a policy table.

5. **`local_id` echo requires the daemon to thread a client-supplied value
   through a relay round-trip.** Confirm the relay's `OK` response gives us
   enough to correlate without it (it gives the event id, which the daemon
   computed at sign time — so yes, and `local_id` is a pure daemon-side map).
   Cheap, but worth stating so nobody adds a tag to the event for it.
