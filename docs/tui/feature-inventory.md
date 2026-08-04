# Buzz Desktop — Complete User-Facing Feature Inventory (TUI Parity Target)

Source: `/data/worktrees/buzz-upmerge` @ branch `upmerge-20260801` (upstream 0.5.4 + fork lanes).
Scope: exhaustive walk of `desktop/src/features/` (30 modules, ~270k LOC), cross-referenced with
`desktop/src/shared/constants/kinds.ts`, `desktop/src/shared/api/*` (the Tauri IPC surface),
and `desktop/src-tauri/src/lib.rs` (`invoke_handler!` registry, ~330 commands).

## Classification legend

| Class | Meaning |
|---|---|
| **TN** — TERMINAL-NATIVE | Ports cleanly to a TUI. Text/list/form-shaped. |
| **TA** — TERMINAL-ADAPTED | Portable, but the interaction must be rethought (drag-drop → keybind, canvas → text, hover → focus). |
| **TH** — TERMINAL-HOSTILE | Needs desktop handoff (audio, camera, image rendering, native OS surface). |
| **DP** — DESKTOP-ONLY-PLUMBING | Not user-facing; app-shell/webview/OS mechanics with no TUI analogue. |

---

## 0. Architectural facts that shape every TUI decision

### 0.1 Two data planes, not one

Every feature reads/writes through exactly one of two paths, and this split is the single
most important fact for a TUI port:

1. **Relay plane** (`shared/api/relayClient*.ts`) — a NIP-42-authenticated WebSocket to the
   community relay. `subscribeLive(filter, cb)` / `requestHistory(filter)` / `publishEvent(event)`.
   Pure Nostr. **A TUI can speak this directly** — no Tauri, no desktop. This is where
   messages, reactions, read-state, channel metadata, projects/git events, observer frames,
   personas/teams projections, reminders, and user-status all live.
2. **Tauri plane** (`shared/api/tauri*.ts` → `invokeTauri(cmd, args)`) — ~330 Rust commands in
   `desktop/src-tauri/src/lib.rs`. Splits further into:
   - (a) **thin relay proxies** (`get_channels`, `send_channel_message`, `add_reaction`,
     `search_messages`, `get_feed`, `get_thread_replies`, …) — the Rust side signs and
     POSTs/WS-publishes. A TUI reimplements these against the relay directly or against
     `buzz-cli`.
   - (b) **genuinely local capabilities** — keyring/identity (`get_nsec`, `sign_event`),
     NIP-44 self-encryption, local SQLite archive, managed-agent process supervision
     (`start_managed_agent_runtime`), local git repos, media pick/upload from disk,
     OS notifications, audio devices, mesh-LLM node. A TUI needs its own equivalents
     (or a headless daemon) for these.
   - (c) **pure desktop chrome** — window vibrancy, tray menu, haptics, deep-link intake,
     webview zoom. No TUI analogue.

### 0.2 Routes = the top-level navigation model

`desktop/src/app/routes.ts` (TanStack virtual file routes):

```
/                                    Home (personal inbox + feed)
/agents                              Agents screen (definitions, instances, personas, teams)
/pulse                               Pulse (social notes feed)
/reminders                           Reminders panel
/settings                            Settings
/workflows  ·  /workflows/$workflowId
/projects   ·  /projects/$projectId
/messages/new                        New-DM composer
/channels/$channelId
/channels/$channelId/posts/$postId   Forum post permalink
```

Everything else is a dialog, sheet, drawer, popover, or right-auxiliary pane hung off these
9 routes. A TUI needs ~9 top-level "screens" plus a modal/overlay stack.

### 0.3 Event-kind master map (from `shared/constants/kinds.ts`)

| Kind | Const | Meaning |
|---|---|---|
| 1 | `KIND_TEXT_NOTE` | NIP-01 note (Pulse, project comments) |
| 5 | `KIND_DELETION` | NIP-09 deletion |
| 7 | `KIND_REACTION` | NIP-25 reaction |
| 9 | `KIND_STREAM_MESSAGE` | legacy chat message |
| 1617 | `KIND_GIT_PATCH` | NIP-34 patch |
| 1618 | `KIND_GIT_PULL_REQUEST` | PR |
| 1619 | `KIND_GIT_PR_UPDATE` | PR update |
| 1621 | `KIND_GIT_ISSUE` | issue |
| 1630–1633 | `KIND_GIT_STATUS_*` | open / merged / closed / draft |
| 1984 | `KIND_REPORT` | NIP-56 report → mod queue |
| 9005 | `KIND_NIP29_DELETE_EVENT` | Buzz-native delete (relay soft-deletes, emits 40099) |
| 9040–9044 | `KIND_MODERATION_*` | ban / unban / timeout / untimeout / resolve-report (relay-validated, never stored) |
| 20002 | `KIND_TYPING_INDICATOR` | ephemeral typing |
| 24200 | `KIND_AGENT_OBSERVER_FRAME` | **encrypted agent observer stream** (NIP-44 agent→owner) |
| 24810 | `KIND_HUDDLE_REACTION` | huddle emoji |
| 30078 | `KIND_READ_STATE` / `CHANNEL_SECTIONS` / `CHANNEL_MUTES` / `CHANNEL_STARS` / `CHANNEL_SORT` | NIP-78 app data, differentiated by `d`-tag |
| 30175 / 30176 / 30177 | `KIND_PERSONA` / `KIND_TEAM` / `KIND_MANAGED_AGENT` | NIP-33 secrets-stripped projections |
| 30300 | `KIND_EVENT_REMINDER` | reminder |
| 30315 | `KIND_USER_STATUS` | NIP-38 user status |
| 30617 / 30618 | `KIND_REPO_ANNOUNCEMENT` / `KIND_REPO_STATE` | NIP-34 repo |
| 30622 | `KIND_DM_VISIBILITY` | relay-signed per-viewer DM hide list |
| 39000 | (relay) channel metadata | **not 41** |
| 39005 | `KIND_CHANNEL_THREAD_SUMMARY` | materialized thread counters |
| 39006 | `KIND_CHANNEL_WINDOW_BOUNDS` | window pagination bounds |
| 40002 | `KIND_STREAM_MESSAGE_V2` | **the** chat message kind |
| 40003 | `KIND_STREAM_MESSAGE_EDIT` | edit overlay (aux) |
| 40007 | `KIND_REMINDER` | remind-me-later |
| 40008 | `KIND_STREAM_MESSAGE_DIFF` | diff message (**own row**, not aux) |
| 40099 | `KIND_SYSTEM_MESSAGE` | join/leave/created system rows |
| 42000 | `KIND_PRODUCT_FEEDBACK` | Send Feedback |
| 43001–43006 | `KIND_JOB_*` | request/accepted/progress/result/cancel/error |
| 44100 / 44101 | `KIND_MEMBER_ADDED/REMOVED_NOTIFICATION` | membership toast |
| 44200 | `KIND_AGENT_TURN_METRIC` | **NIP-AM encrypted per-turn usage** |
| 45001 / 45003 | `KIND_FORUM_POST` / `KIND_FORUM_COMMENT` | forum |
| 46010 | `KIND_APPROVAL_REQUEST` | workflow approval gate |
| 48100–48103 | `KIND_HUDDLE_*` | started / joined / left / ended |

Derived kind sets (all in `kinds.ts`, all load-bearing for a TUI):

- `CHANNEL_MESSAGE_EVENT_KINDS = [9, 40002, 45001, 45003]` — the **unread trigger set**.
  Reactions/edits/diffs/deletions/system are deliberately excluded (they'd create phantom unreads).
- `CHANNEL_TIMELINE_CONTENT_KINDS = [9, 40002, 40008, 40099, 43001..43006, 48100]` — kinds that
  render their **own row**; the history `limit` budget is spent on these.
- `CHANNEL_AUX_EVENT_KINDS = [5, 7, 9005, 40003]` — overlays; fetched separately **by `#e`
  reference over loaded message ids**, not by time window, so a late edit/delete for an old
  visible message still applies.
- `NON_CONVERSATIONAL_UNREAD_KINDS = {40099, 43001..43006, 48100..48103}` +
  `isConversationalUnreadKind(kind)` — system/job/huddle rows are visible but must not count
  toward the unread pill.

**TUI implication:** the two-query timeline fetch (content by time-window, aux by `#e`
reference) and the three-way kind partition (content / aux / non-conversational) must be
reimplemented verbatim or unread counts and edit/delete application will silently diverge.

---

## 1. `messages/` — the chat timeline (229 files, ~49k LOC) — **the heart of the app**

### 1.1 What the user can do

**Read**
- Virtualized (`virtua`) message timeline with day dividers, unread divider, author grouping
  (`messageGrouping.ts`), and per-row reaction strips.
- Infinite upward pagination: `useFetchOlderMessages` / `useLoadOlderOnScroll` /
  `useUpwardPaginationWheel`; anchored scroll preservation (`useAnchoredScroll` +
  `anchoredScrollPolicy`), bottom-settle detection (`useVirtualizedBottomSettle`).
- Timeline retention: old messages are evicted from memory (`timelineRetention.ts`,
  `useTimelineRetention`) to bound RAM on long channels.
- Thread panel (`MessageThreadPanel`) — a right-side reply tree with its own composer;
  independent/detached mode (`useIndependentThreadPanel`, `independentThreadPanel.ts`);
  tree layout (`threadTreeLayout.ts`); thread summary rows on the parent
  (`MessageThreadSummaryRow`, backed by kind **39005**).
- Ancestor backfill for orphan replies (`useLoadMissingAncestors`).
- Diff messages (kind **40008**) rendered as their own row with an expandable unified/side-by-side
  diff viewer (`DiffViewer.tsx`, `parseDiff.ts`, `DiffMessageExpanded`).
- System message rows (kind **40099**) with humanized copy (`systemEventCopy.ts`).
- Ask/answer cards (`AskMessageCard`, `askCard.ts`, `AskAnswersContext`).
- Wave message attachments (`WaveMessageAttachment`, `waveMessage.ts`).
- Agent-snapshot attachment chips (import an agent/team from a message).
- Media: `imeta`-tagged images/video inline, image preload (`timelineImagePreload.ts`),
  link previews (`fetch_link_preview_title`).

**Write**
- TipTap-based rich composer (`useRichTextEditor`) with: bold/italic/strike/code/spoiler
  (`spoilerMark.ts`, `spoilerFormatting.ts`), code blocks with language
  (`codeBlockExtensions.ts`), links (`useLinkEditor`), block formatting on selection
  (`selectionBlockFormatting.ts`, `SelectionFormattingTray`), custom emoji nodes
  (`customEmojiNode.ts`).
- Autocompletes: `@mention` (`MentionAutocomplete`, ranked by `mentionRanking.ts`),
  `#channel` (`ChannelAutocomplete`), `:emoji:` (`EmojiAutocomplete`).
- Attachments: pick from disk, paste, drag-drop; inline image editor
  (`ComposerImageEditor`); `ComposerAttachments` tray.
- Drafts: per-channel/per-thread persistence (`useDrafts`, `useDraftPersistSnapshot`,
  `DraftsPanel`, `DraftDetailPane`, `draftMentionRefs.ts`).
- Reply / edit banner (`ComposerReplyEditBanner`); edit publishes kind **40003**.
- Delete: `DeleteMessageConfirmDialog` → kind **5** or **9005**.
- Reactions: `useReactionHandler`, quick-reaction row (`useQuickReactionEmojis`),
  full picker (`ComposerEmojiPicker`) → kind **7**.
- Typing indicator broadcast (`useTypingBroadcast` → kind **20002**, ephemeral);
  received via `useChannelTyping` → `TypingIndicatorRow`.
- Non-member mention guard (`NonMemberMentionDialog`) — offer to add the mentioned person.
- New-DM screen (`NewMessageScreen`, `useNewMessageRecipients`) at `/messages/new`.
- Thread follow/unfollow (`useThreadFollows`).
- Mark read / mark unread from the row action bar.
- Copy link (`buzz://message?channel=…&id=…`), copy message text.

### 1.2 Kinds

Reads: 9, 40002, 40008, 40099, 43001–43006, 48100 (content rows); 5, 7, 9005, 40003 (aux
overlays, fetched by `#e`); 39005 (thread summary); 39006 (window bounds); 20002 (typing).
Writes: 40002 (send), 40003 (edit), 5/9005 (delete), 7 (react), 20002 (typing).

### 1.3 Tauri commands

`send_channel_message`, `edit_message`, `delete_message`, `add_reaction`, `remove_reaction`,
`get_channel_window`, `get_channel_messages_before`, `get_thread_replies`, `get_event`,
`upload_media`, `pick_and_upload_media`, `pick_and_upload_image`, `upload_media_bytes`,
`download_image`, `download_file`, `fetch_media_bytes`, `copy_image_to_clipboard`,
`copy_text_to_clipboard`, `fetch_link_preview_title`, `fetch_snapshot_bytes`.

### 1.4 Classification

| Sub-feature | Class | Note |
|---|---|---|
| Timeline read, day dividers, grouping, unread divider | **TN** | list rendering |
| Upward pagination / anchored scroll | **TN** | scroll-region math, but text-shaped |
| Thread panel (docked + independent) | **TN** | second pane |
| Diff message viewer | **TN** | diffs are *born* for terminals |
| System messages, Ask cards, job rows | **TN** | |
| Composer: text, mentions, channels, emoji autocomplete | **TN** | readline-style |
| Rich-text marks (bold/italic/code/spoiler/links/code blocks) | **TA** | TipTap → markdown source editing; WYSIWYG is not portable, markdown-with-preview is |
| Drafts | **TN** | |
| Reactions (quick + picker) | **TA** | emoji grid → fuzzy `:shortcode:` picker |
| Typing indicators | **TN** | status line |
| Delete / edit / reply / follow / mark-read | **TN** | |
| Message copy-link, copy text | **TN** | clipboard via OSC 52 |
| Inline images / video / animated avatars | **TH** | needs sixel/kitty graphics or desktop handoff |
| Composer image editor (crop/annotate) | **TH** | |
| Drag-and-drop attach | **TA** | → `:attach <path>` / file picker |
| Timeline virtualization internals (`virtua`, wheel-mode patch, viewport resize) | **DP** | |

---

## 2. `channels/` — channel model, membership, read-state (130 files, ~24k LOC)

### 2.1 What the user can do

- **Channel screen** (`ChannelScreen`, `ChannelPane`) — header (`ChannelScreenHeader`) with
  name/topic/purpose, member bar, join button, status badge, huddle indicator.
- **Channel management sheet** (`ChannelManagementSheet`) — edit name / description / topic /
  purpose; view type (Ongoing vs Temporary/ephemeral), visibility, status (Archived/Ephemeral),
  channel ID copy, ingress row; permissions (`ChannelPermissionsSettings`); type settings;
  Canvas (`ChannelCanvas` — free-text channel doc, `get_canvas`/`set_canvas`);
  moderation actions; archive/unarchive/delete/leave/join.
- **Channel browser** (`ChannelBrowserDialog`) — search-or-create, filter All channels / All
  forums, sort by Alphabetical / Most members / Recent, join inline.
- **Members sidebar** (`MembersSidebar`) — list + search members and agents, per-member card,
  moderation actions, agent controls (start/stop/restart from the roster), remove members.
- **Add bots dialog** (`AddChannelBotDialog`) with three sections: personas, teams, generic;
  reuse guard (`AddChannelBotReuseGuard`) when an equivalent agent already exists;
  quick-bot bar + drag-drop-to-channel (`QuickBotBar`, `useQuickBotDrop`).
- **Respond-to editor** (`EditRespondToDialog`) — which pubkeys an in-channel agent replies to.
- **Forum channels** — a channel type that routes to `ForumChannelContent` instead of the timeline.
- **Thread view mode toggle** (`ThreadViewModeToggle`) — docked vs focus-drawer
  (`FocusThreadDrawer`) vs independent panel; persisted (`threadViewModePreference`).
- **Agent session thread panel** (`AgentSessionThreadPanel`) — the observer transcript rendered
  as a right-hand pane beside the channel.
- **Welcome flow inside a channel** (`WelcomeComposerBanner`, `WelcomeAgentCreateDialog`,
  `useWelcomeAgentCreate`) — first-run "create your first agent here".
- **Membership notifications** (`useMembershipNotifications` → kinds 44100/44101).
- **Ephemeral channels** (`ephemeralChannel.ts`, `EphemeralChannelBadge`) — auto-expiring.

### 2.2 Read-state model (NIP-RS) — **critical, reimplement exactly**

`channels/readState/` implements a CRDT-ish, encrypted, multi-device read frontier:

- Storage: kind **30078** with `d = "read-state:<slotId>"`. Content is **NIP-44 self-encrypted**
  (`nip44_encrypt_to_self` / `nip44_decrypt_from_self`) JSON:
  `{ v: 1, client_id, contexts: Record<contextKey, unixSeconds> }`.
- Context keys: bare `<channelUuid>`, `thread:<64-hex-eventId>`, `msg:<64-hex-eventId>`.
- **Hierarchical frontier rule**: `effective(ctx) = max(merged[ctx], effective(parent(ctx)))`.
  The thread→channel parent link is **not serialized** — it is derived from the event graph
  at evaluation time via a `parentResolver`. Reading a channel therefore covers its threads,
  but reading an ancestor never covers a descendant message.
- Merge across devices: max-wins per context; each device has its own `client_id` and `slot_id`
  (both persisted in localStorage keyed by pubkey).
- Limits: 32 KB plaintext per slot, up to 8 slots (`READ_STATE_MAX_SLOTS`), 10k contexts,
  7-day horizon for `msg:`/`thread:` markers, 5s publish debounce, 500-event fetch limit.
- Local mirror in localStorage (`readStateStorage.ts`) with a 1,000-entry prunable cap.
- Forced-unread override store (`forcedUnreadStore.ts`) for explicit "mark unread".

Unread counting: `unreadChannelCounts.ts`, `useUnreadChannels`, `threadReplyUnreadCounts.ts`,
`threadBadgeCounts.ts`, gated by `isConversationalUnreadKind`.

### 2.3 Kinds
Reads/writes 30078 (`d=read-state:*`), 44100, 44101, 40099, 48100–48103, 9, 39000 (channel meta).

### 2.4 Tauri commands
`get_channels`, `create_channel`, `ensure_starter_channels`, `get_channel_details`,
`get_channel_members`, `update_channel`, `set_channel_topic`, `set_channel_purpose`,
`archive_channel`, `unarchive_channel`, `delete_channel`, `add_channel_members`,
`remove_channel_member`, `change_channel_member_role`, `join_channel`, `leave_channel`,
`get_canvas`, `set_canvas`, `open_dm`, `hide_dm`, `nip44_encrypt_to_self`,
`nip44_decrypt_from_self`, `sign_event`.

### 2.5 Classification

| Sub-feature | Class | Note |
|---|---|---|
| Channel screen / header / join / member bar | **TN** | |
| Channel management sheet (all fields) | **TN** | form → prompt sequence or `:set` commands |
| Channel browser (search / sort / create / join) | **TN** | fuzzy-finder shaped |
| Members sidebar + search + roster agent controls | **TN** | |
| Add-bots dialog (personas / teams / generic) | **TN** | |
| Respond-to editor | **TN** | |
| Read-state / NIP-RS frontier | **TN** | pure logic — **must port verbatim** |
| Unread badges, forced-unread | **TN** | |
| Thread view-mode toggle (docked / drawer / independent) | **TA** | → pane layout modes |
| Channel canvas | **TN** | it's a text doc; `$EDITOR` handoff |
| Quick-bot **drag-drop** onto a channel | **TA** | → `:addbot <name>` or picker |
| Ephemeral channel badge/expiry | **TN** | |
| Agent session thread panel | **TN** | see §7 |

---

## 3. `sidebar/` — navigation, sections, mutes, stars, sort (52 files)

### 3.1 What the user can do
- Channel list grouped into **custom sections** (create/rename/delete/reorder, move channel
  into section, remove from section) — persisted as kind **30078** `d="channel-sections"`.
- **Star** / unstar channels — kind **30078** `d="channel-stars"`.
- **Mute** / unmute channels — kind **30078** `d="channel-mutes"`.
- **Sort preference** (recent / alphabetical / …) — kind **30078** `d="channel-sort"`.
- Context menu per channel: Mark as read, Mark unread, Star, Mute, Move to section,
  New section…, Remove from section, Copy channel name, Copy channel ID, Leave, Archive, Delete.
- **Drag-and-drop reorder** of channels and sections (`SidebarDnd.tsx`).
- DM list with its own sort (`dmSidebarSort.ts`, `useDmSidebarMetadata`).
- Unread overflow "N more unread" jump button (`useUnreadOverflow`, `MoreUnreadButton`).
- Channel activity popover (who's typing / agents working) — `ChannelActivityPopover`,
  `useActiveWorkingChannelsById`.
- Community rail (vertical list of communities with unread dots) — `CommunityRail`.
- Sidebar profile card (own avatar, presence, status) — `SidebarProfileCard`.
- Relay connection card (degraded/reconnecting banner, dismissible) —
  `SidebarRelayConnectionCard`.
- Create-channel dialog (`CreateChannelDialog`, `useCreateChannelForm`).
- Update-available card (`SidebarUpdateCard` from `settings/`).

### 3.2 Classification

| Sub-feature | Class |
|---|---|
| Channel list, sections, stars, mutes, sort, unread badges | **TN** |
| Context menu actions | **TN** |
| Community rail + unread dots | **TN** |
| Activity popover | **TN** |
| Relay connection card | **TN** |
| **Drag-and-drop** reorder | **TA** → keybind move (`K`/`J` in reorder mode) or `:move` |
| Sidebar scroll lock, background target, loading skeleton | **DP** |

---

## 4. `agents/` — the agent control plane (344 files, ~76k LOC) — **the fork's centre of gravity**

This is the single largest module and the one the remote-first thesis lives or dies on.

### 4.1 Object model (four distinct things — do not conflate)

| Object | Kind | What it is |
|---|---|---|
| **Persona** | 30175 | A reusable *definition*: name, avatar, system prompt sections, runtime/harness, model, provider, env vars, MCP servers, respond-to policy. Shareable, catalogable, exportable. |
| **Team** | 30176 | An ordered set of personas deployed together. |
| **Managed agent** (instance) | 30177 | A *running* (or deployable) agent bound to a pubkey + a backend (local process or remote provider). |
| **Relay agent** | (profile) | Any agent pubkey the relay knows about, with a NIP-OA `ownerPubkey`. Ownership is what gates observer decryption. |

All three NIP-33 kinds are published **backend-side as secrets-stripped snapshots**; an inbound
sync hook subscribes to all three and patches local records (`reconcile_inbound_persona_event`).

### 4.2 Where-to-run / deploy UI (`ui/WhereToRunSection.tsx`, `ui/whereToRunIntent.ts`)

**This is the first question in the create flow, deliberately** — the harness comes from the
host's catalog and the models come from the host's harness, so asking it last means answering
dependent questions against the wrong machine.

Flow:
1. **Run target dropdown** — `"local"` (this computer) or the id of a discovered
   **backend provider** (`discover_backend_providers` → `buzz-backend-*` binaries; the shipped
   one is SSH, plus Blox via `sprout-backend-blox`).
2. If a provider is picked → render its **config schema** (`probe_backend_provider` returns
   `{ok, name, version, description, config_schema}`) as a dynamic form (`ProviderConfigFields`).
3. **Discover harnesses on the remote host** — `discover_provider_harnesses` returns
   `RemoteHarnessCatalog { buzzAcp: {path, version} | null, harnesses: RemoteHarness[] }`.
   Each `RemoteHarness` is `{id, label, command, args, env, available, binaryPath, version, exclusive?}`
   describing the REMOTE machine; pinned verbatim onto the agent record at create time
   (nothing re-resolves — a provider-backed agent never spawns locally).
   `exclusive: true` means the entry names a persistent identity on the host, so at most one
   agent may be pinned to it (`exclusiveRemoteHarness.ts` refuses a second).
4. **Probe models on the host** — `probe_provider_models`, merging global env vars under the
   definition's env vars (same merge order the host's `provider_deploy` uses).
5. Failures surface as `HostFailure {message, recovery}` where `recovery` is a
   `ProviderRecovery` (e.g. a Tailscale auth URL) rendered as an actionable button.

Deploy lifecycle: `create_managed_agent` → `start_managed_agent` / `stop_managed_agent`
(provider-backed labels are **"Deploy" / "Shutdown"**; local labels are
**"Spawn" / "Respawn" / "Stop"** — `getManagedAgentPrimaryActionLabel`).
Per-community runtime rows: `list_managed_agent_runtimes`,
`start|stop|restart_managed_agent_runtime`, `put_managed_agent_runtime_lifecycle`,
`reconcile_managed_agent_runtimes`. Lifecycle values:
`starting | listening | waking | ready | failed | stopped`, projected to user-facing
availability `Here | Waking | Needs setup on this device | Unavailable`
(`managedAgentRuntimeStatus.ts`).

**Critical asymmetry for a remote-first TUI:** a provider-backed agent's `"deployed"` status
means only "the deploy call succeeded". `backend_agent_id` is written once and never cleared
(the provider protocol has no undeploy), so a remote agent that died hours ago still reads
`deployed` forever. Liveness for remote agents comes **only** from relay presence
(`managedAgentPresenceStatus` → `presenceLookup[pubkey] ?? "offline"`), while a local agent's
`"running"` is this machine's own process table and stays authoritative.

Also: auto-start on app launch (`set_managed_agent_start_on_app_launch`), auto-restart policy
(`set_managed_agent_auto_restart`, `autoRestartPolicy.ts`), prevent-sleep while agents work
(`usePreventSleep`, `set_prevent_sleep_active`), managed-agent log panel (`get_managed_agent_log`).

### 4.3 Persona / team authoring surface

Dialogs: `AgentDefinitionDialog` (create), `AgentInstanceEditDialog` (edit instance),
`AgentDialog`, `AgentDefaultsDialog` / `AgentDefaultsEditor` (global defaults),
`TeamDialog`, `TeamDeleteDialog`, `PersonaDeleteDialog`, `PersonaCatalogDialog` (browse a
shared catalog), `PersonaShareDialog` / `TeamShareDialog` (share to recipients),
`AddCustomHarnessDialog`, `HarnessCatalogDialog`, `SecretRevealDialog`.

Fields: identity (name, avatar — including generated/minted card art), harness selection
(`AgentHarnessField`, `EditAgentHarnessFields`), LLM provider (`AgentLlmProviderField`,
`ProviderConfigFields`, `PersonaProviderApiKeyField`), model picker
(`ModelPicker`, `PersonaModelCombobox`, `usePersonaModelDiscovery`,
`useRemoteAwareModelDiscovery`, `relayMeshModelPicker`), env vars editor (`EnvVarsEditor`
with missing/required detection and baked-build-env inheritance), MCP servers
(`McpServersSection`), prompt sections (`PromptSectionAccordion`, `personaBehaviorDraft`),
respond-to (`RespondToField` — `owner-only` | `anyone`, plus an allowlist),
advanced fields (`PersonaAdvancedFields`, `EditAgentAdvancedFields`), model tuning
(`buzzAgentModelTuningFields`, effort table), AI configuration mode
(`AgentAiConfigurationMode`, `agentAiConfigurationPolicy`).

Snapshots (portable agent/team bundles):
`export_agent_snapshot` / `encode_agent_snapshot_for_send` /
`preview_agent_snapshot_import` / `confirm_agent_snapshot_import`, and the team equivalents.
Snapshots ride as **message attachments** and as `buzz://` import-from-URL links
(`openSnapshotImportFromUrlEvent`). Avatar PNG is embedded (`snapshotAvatarPng.ts`).

**Agent cards** — AI-minted card art: `card_mint_key_status`, `card_mint_save_openai_key`,
`mint_agent_card`, `save_agent_card`, `list_agent_cards`, `load_agent_card`;
UI `AgentCardMintDialog`, `AgentCardViewerDialog`, `CardMintComposerChip`, `cardMintStore`.

Runtime install: `discover_acp_providers`, `discover_acp_auth_methods`,
`install_acp_runtime` (streams install output lines), `connect_acp_runtime`,
`discover_git_bash_prerequisite`, `discover_managed_agent_prereqs`,
`save_custom_harness` / `delete_custom_harness`.

### 4.4 **Agent activity / observer feed** — exact wire contract

This is the most TUI-relevant subsystem in the app and needs the most precision.

#### 4.4.1 Transport

- Relay kind **24200** (`KIND_AGENT_OBSERVER_FRAME`), addressed `#p = [ownerPubkey]`.
- Content is **NIP-44 ciphertext, agent key → owner pubkey**. Desktop decrypts via
  Tauri `decrypt_observer_event(eventJson)` (needs the owner's private key, so a TUI needs
  its own NIP-44 decrypt or a keyring daemon).
- Subscription (`shared/api/observerRelay.ts`):
  `{ kinds: [24200], "#p": [ownerPubkey], limit: 1000, since: now - 300 }`.
  The 5-minute lookback exists because `session/prompt` is the first frame of a turn and can
  land before the desktop subscribes to an already-running agent. Dedup is on `(seq, timestamp)`.
- **Owner-global ingestion**: `useAgentObserverIngestion()` is mounted once in `AppShell`, not
  per-screen. Ingestion list = local managed agents (real status) ∪ relay agents whose
  profile `ownerPubkey == me` (treated as `deployed`). Frames for agents you don't own never
  arrive at all, because they're `#p`-addressed to their owner.
- **Control channel (upstream)**: `sendAgentObserverControl(agentPubkey, payload)` →
  `build_observer_control_event` (Tauri, signs + encrypts) → `publishEvent`. Payloads:
  - `{type: "cancel_turn", channelId}` — cancel the running turn.
  - `{type: "switch_model", channelId, modelId}` — live model switch; rides the harness's
    cancel-switch-requeue (busy) or invalidate-and-reapply (idle) path. **Fire-and-forget**;
    the outcome arrives asynchronously as a `control_result` observer frame.
  - `{type: "agent_management_request", action: "create"|"update", requestId, request: {...}}`
    — deliberately narrow, no-secret agent create/update requests from inside a channel.

#### 4.4.2 Decrypted frame envelope (`ObserverEvent`, `ui/agentSessionTypes.ts`)

```ts
{ seq: number
  timestamp: string      // RFC 3339, agent-host clock
  kind: string           // frame kind, see table
  agentIndex: number | null
  channelId: string | null
  sessionId: string | null
  turnId: string | null
  startedAt?: string | null
  payload: unknown }
```

#### 4.4.3 Frame kinds (union of `buzz-acp` emitters and desktop consumers)

| `kind` | Consumed how |
|---|---|
| `turn_started` | lifecycle row "Turn started"; records triggering event ids |
| `turn_liveness` | ~every 10s (`BUZZ_ACP_TURN_LIVENESS_SECS`); keeps the working badge alive |
| `turn_ending` / `turn_completed` | terminal — clears the active turn |
| `turn_error` | lifecycle row "Turn error" (`outcome: error`, friendly copy from code) |
| `agent_panic` | lifecycle row "Agent error (crash)" |
| `session_resolved` | lifecycle row "Session ready" |
| `session_config_captured` | invalidates the agent's config-surface query |
| `session_info_update` | session metadata |
| `acp_read` / `acp_write` | **the bulk** — raw ACP JSON-RPC in both directions (see below) |
| `acp_parse_error` | lifecycle row "Wire parse error" |
| `acp_shape` | wire-shape telemetry |
| `raw_json_rpc` | dev-only raw payload card ("Raw ACP payload") |
| `control_result` | async outcome of a `switch_model` control frame; per-agent listeners |
| `agent_claimed` / `agent_returned` | pool lease lifecycle |
| `agent_initialized`, `agent_name`, `agent_pubkey` | identity binding |
| `agent_message_chunk`, `agent_thought_chunk`, `session_update` | streaming content |
| `managed_agent_runtime_lifecycle` | runtime lifecycle mirror into the local record |

#### 4.4.4 ACP method → transcript item mapping (`ui/agentSessionTranscript.ts`)

`acp_write` (desktop/harness → agent):
- `session/new` → **metadata card "System prompt"**, sections parsed from
  `params.systemPrompt` (or `params._meta.systemPrompt.append` for `claude-agent-acp`).
  Section framing: `[Base]` / `[System]` / `[Agent Memory — core]` / `[Channel Canvas]`.
  Rendered with `turnId: null` (standalone, not attached to a turn).
- `session/prompt` → **user message** + **"Prompt context"** metadata card. Prompt text is
  parsed for the user's pubkey and the triggering event id so the transcript row can link
  back to the originating channel message.
- `_goose/unstable/session/steer` → same as prompt, keyed `steer:*`; suppresses the
  `user_message_chunk` echo Goose sends back.
- `session/request_permission` → **permission item**, indexed by JSON-RPC `id`.
- bare `{id, result: {outcome: {outcome, optionId}}}` (no method) → resolves the pending
  permission item's `outcome` field ("Approved (allow_once)" / "Denied (reject_once)" / "Cancelled").

`acp_read` (agent → desktop), `method: "session/update"`, dispatched on `update.sessionUpdate`:

| `sessionUpdate` | Transcript item |
|---|---|
| `agent_message_chunk` | assistant message (coalesced by `messageId`) |
| `user_message_chunk` | user message (`authorPubkey`, nostr event id if `messageId` is 64-hex) |
| `agent_thought_chunk` | "Thinking" thought item |
| `tool_call` | tool item, status `executing` |
| `tool_call_update` | same item id, updated status/args/result/isError |
| `plan` | plan item (with update targeting) |
| `current_mode_update` | status "Mode" |
| `usage_update` | status **"Usage"**, `Tokens: {used}/{size} ($cost currency)` from `update.cost.{amount,currency}` |
| `available_commands_update` | status "Commands available: N" |
| `config_option_update` | status "Config", `name = value, …` |
| anything else with explicit title/text | free-form status; unknown frames are dropped, not guessed |

#### 4.4.5 Render classes (`AgentActivityRenderClass`)

`message | relay-op | file-edit | file-read | skill-read | image | shell | status | thought |
plan | permission | error | generic | raw-rail | suppressed`, each with a
`tone: read | write | admin | neutral`, an `AgentActivityAction {verb, object}`, and a
`source: mcp | shell | acp | harness | fallback`. The classifier
(`agentSessionToolClassifier.ts`) recognises Buzz MCP tools by name — `send_message`,
`get_messages`, `search`, `get_feed`, `list_channels`, `add-channel-member`,
`set-channel-add-policy`, `get_canvas`, `load_skill`, `read_file`, `str_replace`,
`view_image`, `todos`, workflow/issue/repo tools, etc. — and shell/file tools from
`buzz_dev_mcp`. Renderers: `MessageActivity`, `ToolActivity`, `ThoughtActivity`,
`PlanActivity`, `LifecycleActivity`, `RawRailActivity`, `SuppressedActivity`,
`TranscriptActivityItem`, `UserMessageBubble`, plus tool detail blocks
(`ShellCommandBlock`, `FileContentBlock`, `FileEditDiffView`, `TodoToolSummary`,
`ViewImageToolPreview`, `SentMessageContextDialog`).

#### 4.4.6 Store + liveness (`observerRelayStore.ts`, `activeAgentTurnsStore.ts`)

- Live frames: per-agent ring buffer capped at **3000** events. Archive frames live in a
  **separate** per-`(agent, channel)` map with no cap — strict separation so loading deep
  history can never evict live frames.
- Frames for unknown agents are queued (max 100) until that agent registers, then replayed.
- Ordering is `(timestamp, seq)`; a composite watermark makes full-buffer replays idempotent
  and handles harness restarts (seq resets to 1, timestamp keeps climbing).
- **Clock-skew correction**: per-agent offset = running *minimum* of
  `Date.now() - Date.parse(event.timestamp)`. A turn's badge anchor is `startedAt + offset`,
  derived at read time so a later tighter offset retroactively corrects every live turn.
- Turn pruning: remove after `2.5 × 10s` of silence; pause pruning for an agent when ALL its
  turns go quiet at once (that's the frame-stream-down signature) for up to 3 minutes;
  terminal tombstones prevent a late liveness frame resurrecting a completed turn.
  Max 32 concurrent turns per agent (matches `--agents` / `BUZZ_ACP_AGENTS` `1..=32`).
- Derived UI: `TurnLivenessIndicator`, `agentWorkingSignal`, `BotActivityBar`,
  `ChannelActivityPopover`, sidebar working dots, `AgentActivityCard` in Pulse,
  macOS tray agent-activity items.

#### 4.4.7 NIP-AM turn metrics — kind **44200**

`crates/buzz-core/src/agent_turn_metric.rs`. One event per completed turn, NIP-44
agent→owner, decoding to `AgentTurnMetricPayload`:

```
harness: String            (REQUIRED)
model: Option<String>
channelId: Option<String>
sessionId: Option<String>   (REQUIRED when cumulative present)
turnId: Option<String>
turnSeq: Option<u64>        (REQUIRED when cumulative present; strictly increasing per session)
timestamp: String           (RFC 3339, REQUIRED)
turn: Option<TokenCounts>        // this turn's delta
cumulative: Option<TokenCounts>  // session-cumulative as of this turn
TokenCounts = { inputTokens?, outputTokens?, totalTokens?, costUsd?, cacheReadTokens?, cacheWriteTokens? }
stopReason: end_turn | max_tokens | cancelled | error | unknown   // unrecognized → unknown
```
`null` token fields mean **not reported**, not zero. `totalTokens` is provider-reported, not
derived. Unknown fields MUST be ignored.

**Gap worth noting for the TUI:** the desktop currently does **not render** 44200 anywhere.
It only *archives* it (`local-archive` seeds an `owner_p` save subscription including 44200).
The only cost/token surface the user sees is the transcript's `usage_update` status row
(`Tokens: used/size ($cost)`), which comes from the ACP stream, not NIP-AM. **A TUI cost/usage
dashboard built on 44200 would be net-new capability, not parity.**

### 4.5 Classification

| Sub-feature | Class | Note |
|---|---|---|
| Agent/persona/team list, status, start/stop/deploy | **TN** | |
| Where-to-run: provider pick, config schema form, harness discovery, model probe | **TN** | dynamic form → prompt sequence; **the key remote-first surface** |
| Provider recovery actions (e.g. Tailscale auth URL) | **TN** | print URL |
| Runtime lifecycle per community | **TN** | |
| Managed-agent log panel | **TN** | it's a log |
| Persona/team authoring (prompt sections, env vars, MCP, model, respond-to) | **TN** | forms; `$EDITOR` for prompt bodies |
| Observer transcript (messages / thoughts / plans / tools / lifecycle / permissions) | **TN** | **this is a terminal-native surface by nature** |
| Permission request/response resolution | **TN** | inline y/n prompt |
| Tool detail blocks: shell, file content, file edit diff, todos | **TN** | |
| Turn liveness / working badges / clock-skew anchoring | **TN** | |
| Cancel turn / switch model control frames | **TN** | |
| Agent-management requests from channel | **TN** | |
| Snapshot export/import (agent + team) | **TN** | files + `buzz://` URLs |
| NIP-AM 44200 metrics | **TN** | *not currently rendered — opportunity* |
| `ViewImageToolPreview`, agent card art (mint/view), avatar editors | **TH** | image rendering |
| Drag-a-file-onto-window (`useWindowFileDragOver`) | **TA** | |
| Transcript animation preference, bubble overflow measurement, scroll ids | **DP** | |

---

## 5. `home/` — personal inbox + feed (36 files, ~8k LOC) — route `/`

### 5.1 What the user can do
Three-pane layout (`homePaneLayout.ts`): filter menu → inbox list → detail pane, with a
resizable list width (`useResizableInboxListWidth`).

**Filters** (`InboxFilter`): `all | project | mention | thread | needs_action | agent_activity |
reminders | drafts`. UI labels: Mentions, Threads, Needs action, Projects, Drafts, Reminders, Focus.

Each `InboxItem` carries a stable `conversationId` (NIP-10 root for messages, repo-scoped root
for git work) that does **not** change as new replies arrive — it anchors scroll gating, draft
keys, local-reply storage, and selection.

- Read a thread in-place (`InboxDetailPane`, `useInboxThreadContext`,
  `useHomeInboxContextMessages`) and reply without leaving Home.
- Project inbox items (`projectInbox.ts`, `ProjectInboxDetail`, `ProjectInboxDetailPane`) —
  issues/PRs/patches surfaced as inbox conversations.
- Recent notes section (`RecentNotesSection`) and a general feed section (`FeedSection`).
- Auto-selection + selection anchor (`useHomeInboxAutoSelection`, `useInboxSelectionAnchor`).
- Home-scoped read state (`useHomeInboxReadState`) and drafts (`useHomeDrafts`).
- Open a DM from a feed row (`open_dm`).

### 5.2 Kinds
1 (notes), 7, 40007 (reminders), 43001–43006 (jobs), 45001/45003 (forum), 46010 (approvals),
1621/1618/1619/1630–1633 (git), plus `HOME_MENTION_EVENT_KINDS = [9, 40002, 45001, 45003]`
(must stay in sync with the buzz-db Home-feed mention query).

### 5.3 Tauri
`get_feed`, `get_thread_replies`, `get_event`, `open_dm`, `send_channel_message`.

### 5.4 Classification: **TN** throughout (three-pane list/detail is the most terminal-native
shape in the app). Pane resize → **TA** (fixed splits or `Ctrl-w <`/`>`).

---

## 6. `projects/` — Buzz Git (87 files, ~18.5k LOC) — routes `/projects`, `/projects/$projectId`

### 6.1 What the user can do

**Project list** (`ProjectsScreen`, `ProjectsView`): grid or list layout, owner-scope dropdown,
create menu (project / issue / PR / work item), toolbar, contribution graph, activity feed,
overview rail, per-row menu.

**Project detail** (`ProjectDetailScreen`) with workspace tabs:
`Overview | Issues | Pull Requests | Repositories | README | Commits | Contributors | Files`.

- **Issues**: list, filter, create (`CreateIssueDialog`, `CreateProjectIssueDialog`),
  comment (`useCreateProjectIssueCommentMutation`), labels (`projectLabels.ts`),
  status transitions via kinds 1630–1633.
- **Pull requests**: list, detail, files-changed panel
  (`ProjectPullRequestFilesChangedPanel`), inline comments
  (`ProjectPullRequestInlineComments`), reviews (`PullRequestReviewCard`,
  `pullRequestReviews.ts`, `PullRequestReviewersRow`,
  `sign_project_pull_request_review_request`), request review, approve/request-changes,
  **merge** (`MergePullRequestButton`, `merge_project_pull_request`,
  `publish_project_pull_request_merged_status`), conflict recovery
  (`projectPullRequestConflictRecovery.ts`, `open_project_merge_recovery_terminal`).
- **Repositories**: clone (`clone_project_repository`), local repo list
  (`list_project_local_repositories`), push/pull (`push_project_local_repository`,
  `pull_project_local_repository`), sync status (`get_project_repo_sync_status`),
  snapshots + diffs (remote and local: `get_project_repo_snapshot`, `get_project_repo_diff`,
  `get_project_local_repo_snapshot`, `get_project_local_repo_diff`), branch create/delete
  (`create_project_remote_branch`, `delete_project_remote_branch`, `ProjectBranchDialogs`,
  `ProjectBranchActionDialogs`, optimistic branches), commit detail panel + copy SHA,
  ref selection (`useProjectRepositoryRefSelection`), clone URL (`projectCloneUrl.ts`),
  detected languages (`projectLanguages.ts`), **open a terminal in the repo**
  (`open_project_terminal`, `useOpenProjectTerminal`).
- **Agent prompt page** (`ProjectsAgentPromptPage`) — kick an agent off against a project;
  conversation persisted (`projectAgentConversation.ts` + storage).
- Git identity (`get_git_identity`, `useGitIdentity`).
- Delete project.

### 6.2 Kinds
Reads/writes 30617 (repo announcement), 30618 (repo state), 1617 (patch), 1618 (PR),
1619 (PR update), 1621 (issue), 1630/1631/1632/1633 (status), 1 (comments), 9/40002
(project chat), 5 (deletion).

### 6.3 Classification

| Sub-feature | Class |
|---|---|
| Project/issue/PR lists, detail, comments, reviews, merge | **TN** |
| Diffs, files-changed, commit detail | **TN** (terminals were built for this) |
| Repo clone/push/pull/branch/sync | **TN** |
| Open terminal in repo | **TN** — trivially better in a TUI |
| Agent prompt page | **TN** |
| Contribution graph (heat grid) | **TA** — braille/block-char heatmap |
| Grid-vs-list layout toggle | **TA** |
| Rich content rendering (`ProjectRichContent`, README images) | **TA/TH** — markdown yes, images no |

---

## 7. `forum/` — threaded forum channels (13 files)

Forum-type channels render `ForumView` instead of the timeline: post cards
(`ForumPostCard`), a thread panel (`ForumThreadPanel`), a composer with a compact layout
(`ForumComposer`, `ForumComposerCompactLayout`, autocompletes, media status), delete
action menu + confirm dialog. Permalink route `/channels/$channelId/posts/$postId`.
Kinds **45001** (post) / **45003** (comment). Tauri: `get_forum_posts`, `get_forum_thread`.

**Classification: TN** (media attachments → TH).

---

## 8. `search/` — global search + in-channel find (9 files)

- **Topbar search** (`TopbarSearch`) with a results list (`SearchResultItem`) and a prompt
  placeholder. Backed by relay NIP-50 FTS through Tauri `search_messages`.
- **Slack-style operators** (`parseSearchOperators.ts`): `from:`, `in:`, `after:YYYY-MM-DD`,
  `before:YYYY-MM-DD`. Operators must start at a token boundary (deliberately *not* `\b`, so
  `built-in:react` and `https://x.com/in:foo` are not misparsed). `after:` → local start of day
  inclusive; `before:` → one second before local start of day (NIP-01 `until` is inclusive, so
  the named day itself is excluded — Slack-compatible). Invalid operator values stay in the FTS text.
- **In-channel find bar** (`ChannelFindBar`, `useChannelFind`) — incremental find within the
  loaded timeline with next/prev.
- **Gotcha**: an open-ended search with no `kinds` hits the relay p-gate and 403s. The search
  path always scopes kinds.

**Classification: TN** — this is `/`-search, the single most natural TUI interaction.

---

## 9. `moderation/` — reports, bans, timeouts (13 files)

- **Report a message** (`ReportMessageDialog`) → kind **1984** with a type from:
  Spam, Profanity or hate speech, Nudity or sexual content, Illegal content, Impersonation,
  Malware or scam, Other. Persisted to the mod queue.
- **Message moderation menu** (`MessageModerationMenuItems`): delete message, remove author
  from channel, **ban** author (9040) / unban (9041), **time out** author with a duration
  submenu (9042) / lift timeout (9043).
- **Resolve report** (9044) from the moderation queue (`settings/ui/ModerationQueueCard`,
  `settings/lib/moderationQueue.ts`) — list reports, list audit actions, list restrictions.
- **Composer timeout banner** (`ComposerTimeoutBanner`, `timeout.ts`, `timeoutStore.ts`,
  `restrictionState.ts`) — a timed-out user sees the composer disabled with a countdown.
- **Moderation DM** (`moderationDm.ts`) — the 1:1 DM with the relay's own NIP-11 `self`
  pubkey, used to explain an action. The composer is disabled on that channel alone.
  Client-side, best-effort, **fails open** (missing `relaySelf` → composer stays enabled).

Kinds: 1984 (report), 9040–9044 (commands, relay-validated, never stored).
**Classification: TN** throughout.

---

## 10. DMs, gift-wraps, DM visibility

- **DM creation is not NIP-17 client-side.** `open_dm(pubkeys)` publishes a Buzz-native
  **kind 41010** `dm-open` event; the relay replies in its OK payload with a `channel_id`,
  and the desktop then re-reads kind **39000** metadata for that id. A DM is therefore just a
  channel with `channelType: "dm"` and `participantPubkeys`. Messages inside it are ordinary
  kind 40002 events scoped by `h` tag.
- **Hide a DM**: `hide_dm(channelId)` → a `dm-hide` event. Per-viewer visibility is projected
  back by the relay as kind **30622** (`KIND_DM_VISIBILITY`, `d = viewer pubkey`, `h`-tags =
  currently-hidden DM channel ids), observed by `communities/communityUnreadObserver.ts` so
  hidden DMs don't drive unread counts.
- **Gift wrap (kind 1059)** exists in `buzz-core`/`buzz-relay` (accepted, ingested, routed to
  push) and is exercised by the `e2e_nostr_interop` suite for NIP-17 interop — but the
  **desktop UI never constructs or reads 1059**. Desktop DMs are the relay-brokered channel
  model above. A TUI should follow the desktop model, not NIP-17, or DMs will not interoperate
  with the rest of the product.
- DM sidebar metadata + sort: `useDmSidebarMetadata`, `dmSidebarSort.ts`,
  `dmParticipantDisplay.ts`, `dmHuddleMembers.ts`.
- New DM flow: `/messages/new` → `useNewMessageRecipients` → `open_dm` → navigate.

**Classification: TN.**

---

## 11. `huddle/` — voice + TTS (21 files) — **the one genuinely hostile module**

- Start / join / leave / end a huddle (`start_huddle`, `join_huddle`, `leave_huddle`,
  `end_huddle`, `get_huddle_state`, `confirm_huddle_active`).
- Mic capture via `getUserMedia` + an AudioWorklet (`audioWorklet.ts`), PCM pushed to Rust
  (`push_audio_pcm`); audio reconnect (`reconnect_huddle_audio`).
- Voice input mode (push-to-talk vs open mic): `set_voice_input_mode` / `get_voice_input_mode`.
- Output device selection: `list_audio_output_devices`, `set_audio_output_device`,
  `get_audio_output_device`, `useAudioDevices`.
- **STT**: `start_stt_pipeline`, `set_huddle_transcription_enabled`, `check_pipeline_hotstart`;
  transcripts land as live messages.
- **TTS**: `set_tts_enabled`, `speak_agent_message`, `get_tts_settings`, `list_voice_registry`,
  `set_pocket_voice`, `preview_pocket_voice`, `import_pocket_voice`, `delete_pocket_voice`,
  `download_voice_models`, `get_model_status`; live TTS message subscription
  (`ttsLiveMessages.ts`, `useTtsSubscription`).
- **Add an agent to a huddle** (`add_agent_to_huddle`, `AddAgentDialog`, `huddleAgentPicks.ts`,
  `get_huddle_agent_pubkeys`).
- UI: `HuddleBar`, `HuddleIndicator`, `ParticipantList`, `MicControls`, `HuddleAttachment`,
  huddle card in the timeline (kind 48100), reactions (kind 24810).

Kinds: 48100 (started), 48101 (joined), 48102 (left), 48103 (ended), 24810 (reaction),
9/40002 (transcript messages).

**Classification: TH** for everything audio. **TA salvage**: the *huddle card, participant
list, lifecycle rows, transcripts, and agent-in-huddle roster* are all text and can render
in a TUI as read-only state with a "join in desktop" handoff. Starting/joining audio itself
must hand off.

---

## 12. `communities/` — multi-relay switching (34 files)

- Add / edit / remove a community (`AddCommunityDialog`, `EditCommunityDialog`,
  `CommunityEditForm`); a `Community` is `{id, name, relayUrl, token?, pubkey?, addedAt,
  reposDir?}` persisted in localStorage (legacy storage migrated by `legacyCommunityStorage`).
- Community switcher + rail with per-community unread (`useCommunityUnread`,
  `communityUnreadObserver`, `communityMarkRead`); ordering (`applyCommunitiesOrder`).
- Community icons (`useCommunityIcons`, `communityIconCache`, `downscaleIcon`,
  `fetch_workspace_icon`, `CommunityIconSettingsCard`).
- Relay probe before add (`relayProbe.ts`), join-policy fetch (`fetch_join_policy`),
  apply-error screen, change overlay (view transition).
- **Hosted communities (BuilderLab)**: `start_builderlab_login`, `cancel_builderlab_login`,
  `get_builderlab_auth`, `clear_builderlab_auth`, `get_builderlab_nostr_identity`,
  `bind_builderlab_nostr_identity`, `delete_builderlab_nostr_identity`,
  `list_builderlab_communities`, `check_builderlab_community_name`,
  `create_builderlab_community`, `archive_builderlab_community`,
  `unarchive_builderlab_community`, `transfer_builderlab_community`.
  UI: `HostedCommunityOnboarding`, `HostedCommunityCreateFlow`, `HostedCommunitiesSettingsCard`.
- `apply_workspace`, `get_active_workspace`, `validate_repos_dir`.

**Critical mechanic (`useCommunityInit.ts`):** switching communities does **not** reload; it
remounts via `<AppReady key={communityKey} />`. Module-level singletons survive remounts and
must be explicitly reset in `resetCommunityState()` — relay disconnect, rate-limit gate,
drafts, agent observer store, active-agent-turns store, agent working signal, avatar profile
sync, avatar presentations, sidebar relay card, media caches, video player, reaction
hydration, search hit cache, markdown node cache. **A TUI has the identical hazard**: any
module-global cache keyed to a community must be torn down on switch.

**Classification: TN** (community CRUD, switching, unread). Icons → **TH**.
BuilderLab OAuth login → **TA** (device-code / print-URL flow).

---

## 13. `settings/` (53 files) — route `/settings`

Panels: **Profile, Appearance, Notifications, Voice, Agents, Agent defaults, Harnesses,
Remote servers, Compute (mesh), Custom emoji, Channel templates, Hosted communities,
Moderation, Invites, Local archive, Mobile, Experiments, Shortcuts, Updates**.

- Profile card: display name, about, avatar (upload/animated/generated), NIP-05, banner;
  private-key backup row; sign out.
- Appearance: theme Light/Dark/System, accent color, thread layout.
- Notifications: per-slot sounds (`SOUND_SLOTS`, `SLOT_LABELS`, `RECOMMENDED_SOUND_BY_SLOT`,
  `SoundPicker`), desktop notification toggles.
- Voice: TTS/STT settings, pocket voices, model downloads.
- Harnesses: catalog dialog, custom harness form, harness rows/gallery.
- **Remote servers card** (`RemoteServersCard`) — the backend-provider host list.
- Mesh compute (`MeshComputeSettingsCard`): start/stop node, node status, model catalog,
  installed models, download progress, serving usage, share toggle.
- Custom emoji: upload, shortcode, list, remove (kind 30030 emoji set).
- Channel templates: list/create/update/delete/duplicate; apply to a channel.
- Moderation queue card: reports, audit log, restrictions.
- Invites: mint invite, claim invite, invite-link section.
- Local archive card: save subscriptions by kind/scope (see §17).
- Mobile pairing (`start_pairing`, `confirm_pairing_sas`, `cancel_pairing` — NIP-AB SAS).
- Experimental features flags; keyboard shortcuts reference.
- Updates: `UpdateChecker`, `UpdateIndicator`, `SidebarUpdateCard`, `is_auto_update_supported`.
- Send feedback (`SendFeedbackDialog` → kind **42000**).
- Encrypted backup creator + backup test flow.

**Classification:** mostly **TN** (forms/lists/toggles). Sound preview + voice models → **TH**.
Avatar editors → **TH**. Auto-updater → **DP** (a TUI updates via its own channel).
Mobile pairing SAS → **TN** (compare a short string).

---

## 14. `workflows/` (18 files) — routes `/workflows`, `/workflows/$workflowId`

- List workflows (all channels or per channel), create / edit / duplicate / delete via a
  **form builder** (`WorkflowFormBuilder`, `workflowDefinition.ts`, `workflowFormPrimitives`)
  with step cards (`WorkflowStepCard`) and a channel combobox.
- Trigger a workflow manually (`trigger_workflow`); view runs (`getWorkflowRuns`) and a
  **run trace** (`WorkflowRunTrace`).
- **Approvals**: kind **46010** approval requests render as `WorkflowApprovalCard` with
  grant/deny + an optional note (`grant_approval`, `deny_approval`, `get_run_approvals`).
- Webhook secret dialog + custom webhook headers editor (workflows are triggerable at
  `/hooks/{id}`).
- Conditions are evalexpr expressions (server-side, `buzz-workflow`).

**Classification: TN** — YAML-as-code with a form on top; a TUI can expose the YAML directly
plus an approval inbox. The form builder is **TA** (form → guided prompts or `$EDITOR` on YAML).

---

## 15. `pulse/` (13 files) — route `/pulse`

A social notes feed. Tabs: **everyone | people | mine | liked | agents**, plus search.
- Publish a note (kind **1**) — `publish_note`.
- Read timelines: `get_notes_timeline` (follows), `get_global_notes`, `get_user_notes`,
  `get_liked_notes`, `get_note`, `get_note_reactions`.
- Like/react (kind 7), reply, share (`buzz://` note URI), open DM with the author.
- Contact list (follows): `get_contact_list` / `set_contact_list` (kind 3).
- `AgentActivityCard` — agent notes grouped by agent (`groupAgentNotes.ts`).
- Project comments are filtered out of Pulse (`withoutProjectComments`).
- Repo announcements (kind 30617) surface here too.

**Classification: TN.**

---

## 16. `reminders/` (15 files) — route `/reminders`

- "Remind me later" on a message (`RemindMeLaterDialog`, `SnoozeMenu`, `timePresets.ts`)
  → kind **40007**; scheduled reminders as kind **30300** (`KIND_EVENT_REMINDER`).
- Reminders panel with filters (`reminderFilters.ts`), navigation back to the source message
  (`reminderNavigation.ts`), and desktop notifications when due (`useReminderNotifications`).

**Classification: TN.**

---

## 17. `local-archive/` (10 files) — the offline SQLite mirror

A per-user local SQLite archive of relay events, driven by declarative **save subscriptions**.

- `ScopeType = "channel_h" | "owner_p" | "referenced_e"` + a `scopeValue` + a kind list.
- Commands: `archive_events`, `create_save_subscription`, `merge_save_subscription_kinds`,
  `remove_save_subscription_kind`, `list_save_subscriptions`, `delete_save_subscription`,
  `read_archived_events`, `read_archived_observer_events_for_channel`,
  `index_observer_channel_id`, `read_unindexed_observer_rows`,
  `observer_archive_default_enabled`, `agent_metric_archive_default_enabled`.
- `archiveSyncManager.ts` mirrors each subscription onto a live relay subscription and
  enqueues matching events for persistence.
- UI (`LocalArchiveSettingsCard`) presents kind groups: **Messages & posts** (9/40002/45001/
  45003/40008), **Reactions, edits & deletions** (5/7/9005/40003), **Huddle events**
  (48100–48103), **System messages** (40099), **Agent observer frames** (24200),
  **Agent turn metrics** (44200).
- Auto-seeded `owner_p` subscriptions for 24200 (`useObserverArchiveSeed`) and 44200
  (`useAgentMetricArchiveSeed`) so agent history survives the 3000-frame live cap.

**Classification: TN** — and arguably *more* valuable in a TUI (a headless archive daemon +
`grep`-able history is exactly the terminal idiom).

---

## 18. `profile/` (68 files)

- **Own profile**: display name, about, NIP-05, banner, avatar. `get_profile`,
  `update_profile`, `update_profile_at_relay`.
- **Avatar**: upload (`AvatarUpload`, `useAvatarUpload`), image editor with crop/zoom
  (`ProfileAvatarEditor`), **animated avatar capture from the webcam**
  (`AnimatedAvatarCapture`, camera picker, backdrop panel, review nav), custom color panel,
  generated identicon (`jdenticon`), masked badge frame, verified-avatar profile sync
  (`avatarProfileSync`, `avatarPresentationStore`).
- **Other users**: `UserProfilePanel` / `UserProfilePopover` with tabs and sections —
  identity fields, presence + status, Activity log, agent details (for agents: runtime,
  harness, model, last error, **Harness Log**), primary actions (Message, Follow/Unfollow,
  Edit, View, Create card), agent lifecycle actions (Start agent, Restart), archive confirm.
- **Identity binding**: bind a Nostr identity (`nostrIdentityBinding.ts`,
  `NostrBindConsentDialog`, `sign_nostr_identity_binding`, `nostrBindCallback`).
- **User search** (`search_users`, `userCandidateSearch.ts`), batch profile fetch
  (`get_users_batch`), profile cache sync.
- **Activity carousel** (`profileActivityCarousel.ts`, `profileActivityFeedScope.ts`,
  `profileActivityAgent.ts`) — recent notes/messages by that user.
- Snapshot export from a profile (`UserProfileSnapshotExportDialog`).

**Classification:** text fields, actions, activity log, agent details → **TN**.
Avatar upload/editor/webcam capture/identicon rendering → **TH**.

---

## 19. `presence/` + `user-status/`

- **Presence**: `get_presence`, live kind-20001-ish presence events, `usePresenceSubscription`,
  `useSetPresenceMutation`, `usePresenceSession`; automatic status derived from OS idle
  (`get_os_idle_seconds`, `resolveAutomaticPresenceStatus`); `PresenceBadge`, dot/chip classes.
- **User status** (kind **30315**, NIP-38): `SetStatusDialog` — emoji + text + expiry;
  `useUserStatusQuery` / `useUserStatusSubscription` / `useSetUserStatusMutation`;
  `StatusEmoji` rendered next to names.

**Classification: TN** (emoji renders as a glyph).

---

## 20. `community-members/` (7 files)

- List community members (`list_relay_members`), roles, add member (`AddMemberDialog`,
  `add_relay_member`), remove (`ConfirmRemoveDialog`, `remove_relay_member`), change role
  (`change_relay_member_role`), own membership (`get_my_relay_membership`),
  membership requirement probe (`relay_requires_membership`).
- **Invites**: `CommunityInviteDialog`, `InviteLinkSection` — mint an invite
  (`mintInvite`), claim one (`claimInvite`), parse pasted invite input
  (`parseInviteInput`), join-policy accept (`getJoinPolicy`, `acceptJoinPolicy`).

**Classification: TN.**

---

## 21. `custom-emoji/` (4 files)

Each member publishes their **own** kind **30030** parameterized-replaceable event
(`d = "buzz:custom-emoji"`), signed as themselves. The community palette is the **union** of
every member's 30030 set (`unionCustomEmoji`). Upload an image, pick a shortcode
(`suggestShortcodeFromFilename`, `normalizeShortcode`), list, remove. Rendered in the picker
(`EmojiPicker` + emoji-mart), in the composer (`customEmojiNode`), and in reactions
(`reactionEmojiUrl`).

**Classification: TA** — shortcode picker and `:name:` text are TN; the rendered custom-emoji
*image* is TH (falls back to showing `:shortcode:`).

---

## 22. `channel-templates/` (2 files)

`list_channel_templates`, `create_channel_template`, `update_channel_template`,
`delete_channel_template`, `duplicate_channel_template`; `useApplyTemplate` seeds a new
channel with a preset (topic/purpose/canvas/bots). Settings UI in
`ChannelTemplatesSettingsCard`. **Classification: TN.**

---

## 23. `agent-memory/` (4 files) — Engrams

`get_agent_memory` returns an `AgentMemoryListing` rendered as a memory graph
(`buildMemoryGraph`) in `MemorySection` on the agent's profile panel.

**Ownership gate (important and subtle):** engrams are NIP-44 encrypted to the **owner's**
pubkey and decrypted with the owner's key — never the agent's seckey. So
`viewerIsOwner = isCurrentUserOwner (declared NIP-OA owner from the agent's kind:0)
|| useIsManagedAgent (do I hold the seckey locally?)`. The two diverge exactly for a
**remote-owned agent**: the owner runs it elsewhere, holds no local seckey, but legitimately
owns and can read its memory. **A remote-first TUI must use the declared-owner half.**

**Classification: TN.**

---

## 24. `identity-archive/` (1 file)

Relay kind **13535** snapshot of archived identities. `list_archived_identities`,
`archive_identity`, `unarchive_identity`, `resolve_oa_owner`. Drives an "Archived" flair on
profiles and hides retired agents. **Classification: TN.**

---

## 25. `mesh-compute/` (10 files)

Local mesh-LLM node (feature-gated `mesh-llm`, macOS-focused). Start/stop node
(`mesh_start_node`, `mesh_stop_node`), node status, model catalog + installed models,
download progress, serving usage (how much of your machine others used), a share toggle.
Mesh models appear as an LLM-provider option in the agent model picker
(`relayMeshModelPicker`, `classifyModelRef`).

**Classification: TN** for status/toggles/usage. The node itself is a local daemon —
**DP** from the TUI's perspective (it can control it, not host it).

---

## 26. `onboarding/` (66 files, ~16k LOC)

- **First-run flow** (`OnboardingFlow`): identity → profile → avatar → backup → download key
  → setup → default config. Steps: `ProfileStep`, `AvatarStep`, `BackupStep`,
  `DownloadKeyStep`, `SetupStep`, `DefaultConfigStep`.
- Identity: start new, or **import an existing nsec/ncryptsec** (`NostrKeyImportForm`,
  `keyImportInput.ts`, `import_identity`, `persist_current_identity`).
- **Encrypted backup**: `generate_backup_passphrase`, `create_ncryptsec_backup`,
  `verify_ncryptsec_backup`, `save_ncryptsec_copy`; a backup *test* flow that makes you
  restore before continuing; masked nsec display; password timeline.
- **Community onboarding** (`CommunityOnboardingFlow`, `communityOnboarding.tsx`) —
  join policy notice, membership-denied screen, pending-invite gate, invite redeem form.
- **Machine onboarding** (`MachineOnboardingFlow`, `machineOnboarding.ts`) — set this
  computer up as an agent host; runtime selection (`onboardingRuntimeSelection.ts`),
  runtime install with error tooltips, harness marks, **remote-run notice**
  (`remoteRunNotice.ts`), agent readiness gate.
- **Welcome kickoff stage** (`WelcomeKickoffStage`, `welcomeCanvas.ts`, `LandingBees.tsx`) —
  the animated landing.
- Recovery / keyring-locked / relaunch-required / reset-failed screens.

**Classification:** identity, import, backup, invites, machine setup, runtime install → **TN**.
Avatar step → **TH**. Landing animation + slide transitions → **DP**.

---

## 27. `notifications/` (12 files)

- `shouldNotifyForEvent(event, me, {participatedRootIds, followedRootIds, authoredRootIds,
  mutedRootIds, mutedChannelIds, channelId})` — notify on: broadcast replies (always),
  direct `p`-tag mentions, replies in threads you authored/participated in/follow; suppressed
  by muted roots and muted channels.
- Desktop OS notifications (`show_native_notification`, `desktop.ts`,
  `use-feed-desktop-notifications`), per-slot sounds (`sound.ts`), notification formatting
  (`notificationFormat.ts`), Home badge count (`homeBadge.ts`).
- Approval-request (46010) and job-lifecycle (43001–43006) notifications.

**Classification:** the *decision logic* is **TN** and must port verbatim. OS notification
delivery + sounds → **TA** (terminal bell / `notify-send` / OSC 9) or **TH** on some terminals.

---

## 28. Cross-cutting: relay connection lifecycle (`shared/api/relay*`)

Not a feature module but user-visible, and a TUI needs all of it:

- NIP-42 auth (`create_auth_event`), preconnect, reconnect controller with backoff policy,
  reconnect **replay** (re-issues history pages + live subscriptions after a drop),
  closed-policy classification (service restart vs fatal), stall watchdog, rate-limit gate
  (parses relay rate-limit hints and blocks writes until the window clears),
  degraded-connection detection, query invalidation on reconnect, auto-heal,
  `relay_reconnect_hook` (an external hook the desktop can call on reconnect).
- User-visible surfaces: `RelayConnectionOverlay`, `SidebarRelayConnectionCard`,
  `CommunityApplyErrorScreen`.
- **p-gate**: any REQ without explicit `kinds` returns 403. Every filter must name kinds.

**Classification: TN** (status line + reconnect banner).

---

## 29. Explicit DESKTOP-ONLY-PLUMBING inventory

| Item | Where |
|---|---|
| Window vibrancy, title-bar double-click, window drag | `set_window_vibrancy`, `title_bar_double_click`, `useTauriWindowDrag` |
| macOS tray menu + tray agent activity | `tray_menu::*`, `useTrayMenu`, `useAppShellTrayMenu` |
| Haptics | `perform_sidebar_default_haptic` |
| Webview zoom (rem-scaling) | `useWebviewZoomShortcuts` |
| Deep-link intake | `take_pending_community_deep_link`, `acknowledge_pending_community_deep_link`, `shared/deep-link.ts` |
| Auto-updater | `is_auto_update_supported`, `UpdateChecker`, `UpdaterProvider` |
| Reload shortcut | `useReloadShortcut` |
| View transitions / community change overlay | `communityViewTransition.ts`, `CommunityChangeOverlay` |
| Virtualization internals | `virtuaWheelModePatch`, `useVirtualizedViewportResize`, `useComposerHeightPadding`, `rowHeightEstimate` |
| Theme surfaces / grainient background | `BuzzThemeSurfaces`, `ThemeGrainientBackground` |
| Animation/transcript animation preferences | `transcriptAnimationPreference`, `OnboardingSlideTransition` |
| Media proxy port | `get_media_proxy_port` |
| Restart-after-mesh-shutdown hard exit | `lib.rs` `RunEvent::Exit` |

---

## 30. Keyboard shortcuts already defined (free parity wins)

From `shared/lib/keyboard-shortcuts.ts`: Quick search, Browse channels, New direct message,
New channel, Settings, Go back, Go forward, Find in channel, Home, Toggle sidebar,
Mark as read, Mark all as read, Zoom in/out/reset, Send message, New line, Publish note,
Close dialog, Push to talk, Bold, Italic, Strikethrough, Inline code, Insert link.

A TUI inherits the *semantics* of every one of these; only zoom and push-to-talk don't carry.

---

## 31. Master classification table

| # | Module | Files | Class | Notes |
|---|---|---|---|---|
| 1 | `messages` | 229 | **TN** (rich text **TA**, images/editor **TH**) | Core. Two-query timeline (content by window, aux by `#e`) must port verbatim. |
| 2 | `channels` | 130 | **TN** (thread modes **TA**, quick-bot DnD **TA**) | NIP-RS read-state is the highest-risk port. |
| 3 | `sidebar` | 52 | **TN** (DnD reorder **TA**) | Sections/stars/mutes/sort all kind 30078 `d`-tagged. |
| 4 | `agents` | 344 | **TN** (card art / image tools **TH**) | Where-to-run + observer transcript are the flagship TUI surfaces. |
| 5 | `home` | 36 | **TN** (pane resize **TA**) | Three-pane inbox; 8 filters. |
| 6 | `projects` | 87 | **TN** (contrib graph **TA**, README images **TH**) | Git/PR/issue/diff — terminal's home turf. |
| 7 | `forum` | 13 | **TN** | Kinds 45001/45003. |
| 8 | `search` | 9 | **TN** | Slack operators; must always scope `kinds` (p-gate). |
| 9 | `moderation` | 13 | **TN** | 1984 + 9040–9044. |
| 10 | DMs (in `channels`/`messages`) | — | **TN** | kind 41010 `dm-open`, **not** NIP-17 gift wrap. |
| 11 | `huddle` | 21 | **TH** (cards/roster/transcripts salvageable as **TA**) | Audio must hand off to desktop. |
| 12 | `communities` | 34 | **TN** (icons **TH**, BuilderLab OAuth **TA**) | Singleton-reset hazard on switch applies to a TUI too. |
| 13 | `settings` | 53 | **TN** (voice/avatar **TH**, updater **DP**) | 19 panels. |
| 14 | `workflows` | 18 | **TN** (form builder **TA**) | 46010 approvals inbox. |
| 15 | `pulse` | 13 | **TN** | kind 1 + kind 3 contacts. |
| 16 | `reminders` | 15 | **TN** | 40007 + 30300. |
| 17 | `local-archive` | 10 | **TN** | Better in a TUI than on the desktop. |
| 18 | `profile` | 68 | **TN** (avatars/webcam **TH**) | Agent details + harness log are TN. |
| 19 | `presence` | 4 | **TN** | OS-idle input needs a TUI equivalent. |
| 19b | `user-status` | 3 | **TN** | kind 30315. |
| 20 | `community-members` | 7 | **TN** | Members + invites. |
| 21 | `custom-emoji` | 4 | **TA** | Shortcodes TN; rendered images TH. |
| 22 | `channel-templates` | 2 | **TN** | |
| 23 | `agent-memory` | 4 | **TN** | Declared-owner gate is the remote-first-correct one. |
| 24 | `identity-archive` | 1 | **TN** | kind 13535. |
| 25 | `mesh-compute` | 10 | **TN** control surface / **DP** node hosting | |
| 26 | `onboarding` | 66 | **TN** (avatar step **TH**, landing anim **DP**) | Machine onboarding matters for remote-first. |
| 27 | `notifications` | 12 | **TN** logic / **TA** delivery | `shouldNotifyForEvent` ports verbatim. |
| 28 | `chat` | 1 | **TN** | Single `ChatHeader.tsx` — shared channel-header chrome: channel-type glyph (hash / lock-private / bot / git / doc / activity), name, copy-id, update indicator. Reused across screens. |
| 29 | Relay lifecycle (`shared/api`) | ~40 | **TN** | Reconnect replay + rate-limit gate are mandatory. |
| 30 | App shell / window / tray / zoom / deep links | ~20 | **DP** | |

### Rollup

- **TERMINAL-NATIVE**: 24 of 30 modules, and ~85% of the user-facing surface by feature count.
- **TERMINAL-ADAPTED**: rich-text authoring, drag-and-drop (sidebar reorder, quick-bot,
  attachments), reaction/emoji pickers, thread-layout modes, pane resizing, form builders
  (workflow, provider config), contribution graph, notification delivery, BuilderLab OAuth.
- **TERMINAL-HOSTILE**: huddle audio (start/join/mic/TTS/STT), all image rendering
  (inline media, avatars, agent card art, `view_image` tool previews, community icons,
  custom-emoji glyphs), webcam animated-avatar capture, the composer image editor.
- **DESKTOP-ONLY-PLUMBING**: window chrome, tray, haptics, zoom, deep-link intake,
  auto-updater, virtualization internals, view transitions, theme surfaces.

### Highest-risk ports (get these wrong and everything downstream is subtly wrong)

1. **NIP-RS read-state** (§2.2) — encrypted 30078 slots, hierarchical frontier with a
   graph-derived parent resolver, multi-device max-merge, 32 KB/8-slot limits.
2. **Timeline kind partition** (§0.3) — content vs aux vs non-conversational; aux fetched by
   `#e` reference, not by time window.
3. **Observer frame → transcript state machine** (§4.4.4) — the ACP method dispatch, the
   permission request/response correlation by JSON-RPC id, the Goose steer/echo suppression.
4. **Turn liveness + clock-skew anchoring** (§4.4.6) — running-minimum offset, terminal
   tombstones, all-quiet prune pause.
5. **Remote-agent liveness asymmetry** (§4.2) — `deployed` never clears; only relay presence
   knows.
6. **Community-switch singleton reset** (§12).
7. **Relay p-gate** — every REQ must name `kinds`.

### Net-new opportunities a TUI could claim (not parity — upside)

- **NIP-AM (44200) cost/token dashboard** — the payload is fully specified and archived, but
  the desktop renders none of it.
- **Grep-able local archive** — the SQLite mirror is already there.
- **Headless/remote-first agent supervision** — the desktop's local-process supervision is the
  part a TUI on the VPS would replace, not reimplement.
