# Buzz TUI — UX Patterns

Extracted 2026-08-04 from three sources:

1. `/data/tmp-buzz-planning/tui-research.md` — stack research (OpenTUI / opencode /
   FrankenTUI architecture + verdicts).
2. **opencode** — `/tmp/tui-patterns/opencode` (shallow clone, `sst/opencode` →
   redirects to `anomalyco/opencode`). All file:line references below are verified
   against that clone. License **MIT** — patterns *and* code are reusable.
3. Local skills — `~/.claude/skills/tui-glamorous/`, `~/.claude/skills/frankentui/`,
   `~/.claude/skills/tui-inspector/`.

## LICENSE GATE — read before copying anything

| Source | License | What we may take |
|---|---|---|
| opencode (`packages/tui`) | MIT | **Patterns and code.** Vendoring individual files is legally clean (the package is `"private": true` on npm, but the source is MIT). |
| OpenTUI (`@opentui/*`) | MIT | Patterns and code; consume as a normal npm dependency. |
| **FrankenTUI** | `LicenseRef-MIT-OpenAI-Anthropic-Rider` | **PATTERNS ONLY. No code reuse, no vendoring, no `cargo add`, no benchmarking/eval-harness use.** The rider grants **no rights to Anthropic, its affiliates, or anyone acting "for the benefit of" them**, is viral on redistribution, and self-terminates on breach. Buzz is an Anthropic-agent platform in a Block OSS repo. Everything below marked **[FT-pattern]** is a *described idea*, restated in our own words — no `ftui-*` crate may appear in `Cargo.toml`. |
| tui-glamorous skill (Charm ecosystem) | skill is guidance; Charm libs are MIT | Patterns; Charm code is Go and irrelevant to our stack, so treat as patterns only. |

Everywhere below, a `crates/ftui-*` path is a *citation of where the idea was
observed*, never an instruction to read-and-copy that code into Buzz.

---

## 0. What this document is for

The research doc's ranked recommendation is **(b2): a TypeScript/OpenTUI front end
over a Rust `buzz-daemon` sidecar**, with **(a) pure ratatui** as the hedge. The
patterns below are deliberately written to be **front-end-agnostic** — each one is
a behavioural contract, not a code snippet — so that if we swap OpenTUI/Solid for
ratatui against the same daemon, the UX spec survives intact.

Two Buzz-specific facts shape every pattern:

- Buzz is **multiplayer chat**, not a single-session agent REPL. The unit of
  navigation is *community → channel → thread*, not *session*. Presence, unread
  state, reactions, DMs, and multi-community switching have **no analogue** in
  opencode and must be designed, not ported.
- Buzz agents are **remote** (VPS-hosted). The TUI is a client of a daemon that may
  live on another machine. Every pattern that touches connectivity must degrade
  visibly rather than hang.

---

## PATTERN LIST (headline)

Twenty-six patterns in nine groups:

| # | Group | Patterns |
|---|---|---|
| **1** | Keybinding grammar | P1 declarative keybind table · P2 leader + timed sequences · P3 command↔binding indirection · P4 unbindable/multi-bind values · P5 which-key discoverability |
| **2** | Focus & panes | P6 mode stack (not booleans) · P7 managed-textarea layer · P8 dialog stack + focus restoration · P9 pane focus enum + hit-region cache **[FT-pattern]** |
| **3** | Composer | P10 parts model with extmarks · P11 @-mention trigger mechanics · P12 frecency ranking · P13 prompt history · P14 draft stash · P15 external-editor handoff |
| **4** | Streaming render | P16 store-reconcile (not append-to-log) · P17 16 ms batched flush · P18 sticky-bottom scroll · P19 incremental markdown **[FT-pattern]** |
| **5** | Command palette & dialogs | P20 one dialog primitive, N dialogs · P21 palette fed by the keybind table · P22 fuzzy + suggested + category |
| **6** | Theming | P23 semantic token set + derived syntax palette |
| **7** | Attention | P24 focus-aware notification/sound with skip reasons |
| **8** | Resilience | P25 responsive tiers + tiny fallback + degradation **[FT-pattern]** |
| **9** | Connectivity | P26 reconnect/backoff surfaced in chrome (Buzz-original) |

Full detail follows.

---
## Group 1 — Keybinding grammar

opencode's keybinding layer is the single most transferable subsystem found. It is
not "a switch statement in the key handler"; it is a **four-layer grammar**:

```
  config/keybind.ts   Definitions{}    name -> { default: BindingValue, description }
                      CommandMap{}     name -> dotted command id
        │
        ▼
  @opentui/keymap     layers, modes, pending sequences, binding expanders
        │  addons: registerTimedLeader, registerEscapeClearsPendingSequence,
        │          registerBackspacePopsPendingSequence,
        │          registerManagedTextareaLayer, registerBaseLayoutFallback,
        │          registerCommaBindings
        ▼
  useBindings(...)    per-component registration: { target, enabled, commands[], bindings[] }
        │
        ▼
  consumers           command palette · which-key panel · help dialog · slash commands
```

### P1 — One declarative keybind table, `{default, description}` per entry

`packages/tui/src/config/keybind.ts:44-232` — ~230 entries, each
`keybind(default, description)`. Nothing else in the app hardcodes a key.

```ts
app_exit:      keybind("ctrl+c,ctrl+d,<leader>q", "Exit the application"),
command_list:  keybind("ctrl+p",                  "List available commands"),
session_new:   keybind("<leader>n",               "Create a new session"),
input_newline: keybind("shift+return,ctrl+return,alt+return,ctrl+j", "Insert newline in input"),
```

The `description` field is the load-bearing part: because every binding carries
human text, the **command palette, the which-key panel, and the help dialog are all
generated from this one table for free** (`keybind.ts:253-259` derives
`Descriptions`; `keybind.ts:437-444` `bindingDefaults()` back-fills `desc` onto any
binding that lacks one).

**Buzz mapping** (illustrative, ~60 entries to start):

| Buzz command | Suggested default | Notes |
|---|---|---|
| `community_switch` | `<leader>o` | new axis; no opencode analogue |
| `channel_list` | `<leader>k` | Slack muscle memory (`ctrl+k` reserved for palette on some terms) |
| `channel_next` / `channel_prev` | `alt+down` / `alt+up` | |
| `channel_next_unread` | `alt+shift+down` | |
| `thread_open` / `thread_close` | `enter` / `escape` | in message-focus mode |
| `dm_list` | `<leader>d` | |
| `message_react` | `<leader>e` | opens emoji picker dialog |
| `message_reply` | `r` | message-focus mode only |
| `message_copy` | `<leader>y` | mirrors `messages_copy` |
| `mark_read` | `<leader>m` | |
| `agent_list` | `<leader>a` | keep opencode's binding |
| `command_list` | `ctrl+p` | keep |
| `app_exit` | `ctrl+c,ctrl+d,<leader>q` | keep |

**Do not** re-derive the `input_*` family — copy opencode's 40-odd editing bindings
(`keybind.ts:170-201`) verbatim. They encode emacs+readline+macOS conventions that
users already have in their fingers (`ctrl+a/e` line home/end, `alt+f/b` word
motion, `ctrl+w` delete-word-back, `ctrl+k/u` kill-to-end/start).

### P2 — Leader key + timed sequences

`leader: keybind("ctrl+x", ...)` (`keybind.ts:43`, `LeaderDefault = "ctrl+x"`), and
bindings reference it with a `<leader>` token that is expanded at registration
(`keymap.tsx:196-202` `registerTimedLeader(keymap, { trigger, name: "leader",
timeoutMs: config.leader_timeout })`).

Three companion behaviours make sequences feel right — all are opencode/OpenTUI
addons, all worth reproducing whatever front end we pick:

- **Timeout**: a pending leader expires after `leader_timeout` ms rather than
  latching forever.
- **Escape clears the pending sequence** (`registerEscapeClearsPendingSequence`).
- **Backspace pops one token off the pending sequence**
  (`registerBackspacePopsPendingSequence`) — so a mistyped `<leader>s` can be
  corrected to `<leader>n` without starting over.

`useLeaderActive()` (`keymap.tsx:243`) exposes "a leader is pending" as a reactive
signal so the status bar can show it. **Show pending state in chrome** — an
invisible modal state is the #1 way keybinding grammars feel broken.

### P3 — Name ↔ command indirection (`CommandMap`)

`keybind.ts:260-425` maps every config name to a dotted command id:
`session_new → "session.new"`, `command_list → "command.palette.show"`,
`messages_page_up → "session.page.up"`.

Why this indirection matters, concretely:

- The **config surface** (what users write in their config file) is stable and
  snake_case; the **dispatch surface** (what components register and the palette
  fires) is namespaced and can be refactored freely.
- Commands can be dispatched **without a key** —
  `keymap.dispatchCommand(entry.command.name)` from the palette
  (`command-palette.tsx:57`) or from a slash command (`keymap.tsx:289`). So every
  capability is reachable three ways: key, palette, slash — with one definition.
- A command may have **zero** bindings (`keybind("none", ...)`, used for ~40 entries
  like `session_share`, `mcp_list`) and still be palette-reachable. This is the
  right default for a chat app's long tail (`invite`, `set-topic`, `export`).

### P4 — Binding *values* are a small union, not a string

`keybind.ts:16-35`: a binding value is
`false | "none" | string | KeyStroke | BindingObject | Array<…>`, where
`BindingObject = { key, event?: "press"|"release", preventDefault?, fallthrough? }`.

Consequences we want:

- **Unbind** = `false` or `"none"`. Users can disable a default without inventing a
  sentinel key.
- **Multi-bind** = comma string (`"ctrl+g,home"`) or array.
- **`preventDefault: false`** lets a binding run *and* let the terminal/textarea also
  handle it — opencode uses exactly this for paste:
  `input_paste: keybind({ key: "ctrl+v", preventDefault: false }, …)` (`keybind.ts:172`).
- Unknown keys in user config are a **hard error listing the offenders**
  (`keybind.ts:428-435` `unknownKeys` → `throw new Error("Unrecognized keybind…")`),
  not a silent ignore.
- Key aliases are normalised by a **binding expander** rather than by string
  munging at every call site (`keymap.tsx:118-133`: `enter→return`, `esc→escape`,
  `pgup→pageup`, `pgdown→pagedown`).

### P5 — Discoverability: which-key panel + help overlay

`feature-plugins/system/which-key.tsx` (608 lines) renders the pending-sequence
continuation set as a dockable/overlay panel — grouped by category, tab-navigable
between groups, scrollable, with its own bindings under `ctrl+alt+*`
(`keybind.ts:230-241`). Layout constants worth stealing directly:
`MIN_COLUMN_WIDTH 28 / MAX_COLUMN_WIDTH 44`, `PANEL_HEIGHT_RATIO 0.3` clamped to
`8..16` rows, dock-vs-overlay layout persisted in KV.

**[FT-pattern]** The FrankenTUI showcase contract states the same requirement more
strongly and adds one thing opencode does not do: *the help overlay must **merge**
global and screen-specific keybindings* — a user pressing `?` in the thread pane
should see thread keys **and** app keys in one list, sectioned. Adopt the merge
requirement; implement it ourselves from our own keybind table.

**Buzz addition — the leader menu is the chat verb surface.** In a chat client the
leader menu is where `#channel`, `@dm`, `+react`, `!invite` live. Group them by
category in which-key so `<leader>` alone renders a legible menu of chat verbs.

---

## Group 2 — Focus and pane model

### P6 — Modal focus is a **mode stack**, never booleans

`keymap.tsx:53-98` `createOpencodeModeStack`. The mechanism:

```ts
keymap.setData("opencode.mode", "base")                    // base mode
keymap.registerLayerFields({
  mode(value, ctx) { ctx.require("opencode.mode", value) } // layers declare their mode
})
push(mode) → stack.push({id: Symbol(mode), mode}); update() → returns a pop closure
```

`update()` sets the keymap's mode data to `stack.at(-1)?.mode ?? "base"`. Layers that
declare `mode: "modal"` are only live while `"modal"` is on top. Every push returns
its own disposer and pops **by identity** (`findIndex(item => item.id === id)`), so
out-of-order cleanup can never corrupt the stack — the exact bug that boolean
`isModalOpen` flags produce.

Callers are one-liners tied to component lifetime:

```ts
createEffect(() => {                              // ui/dialog.tsx:80-84
  if (store.stack.length === 0) return
  const popMode = modeStack.push("modal"); onCleanup(popMode)
})
createEffect(() => {                              // autocomplete.tsx:109-113
  if (!store.visible) return
  const popMode = modeStack.push("autocomplete"); onCleanup(popMode)
})
```

**Buzz mode set** (proposed): `base` · `composer` · `autocomplete` · `modal` ·
`palette` · `thread` · `message-select` · `search`. The rule that makes a chat TUI
feel correct: **typing in the composer must not fire global single-letter keys.**
That is exactly what mode-scoping plus P7 delivers.

**[FT-pattern]** FrankenTUI states the same idea as an *enum-driven focus
management* invariant with a modal priority layer, plus a **focus-restoration**
requirement (save focus before overlay, restore on dismiss). opencode implements
that restoration explicitly — see P8.

### P7 — The managed-textarea layer

`registerManagedTextareaLayer(keymap, renderer, { enabled, bindings })`
(`keymap.tsx:203-207`) installs the ~40 `input.*` bindings **only when a real
textarea has focus**, gated by:

```ts
function hasManagedTextareaFocus(renderer) {          // keymap.tsx:170-173
  const editor = renderer.currentFocusedEditor
  return editor instanceof TextareaRenderable && !(editor instanceof InputRenderable)
}
```

Note the `!(… instanceof InputRenderable)` — single-line inputs inside dialogs get
*different* editing bindings than the multi-line composer. That distinction is worth
preserving: in Buzz, the channel-search box and the message composer should not
share `input.newline`.

### P8 — Dialogs are a stack with focus restoration

`ui/dialog.tsx:66-186`. The dialog context holds `stack: {element, onClose}[]` plus a
`size`. Three behaviours to copy:

1. **Focus save/restore.** On first push, `focus = renderer.currentFocusedRenderable;
   focus?.blur()` (`dialog.tsx:151-153`). On close, `refocus()` walks the live tree to
   verify the saved renderable still exists (`!focus.isDestroyed` + a `find()` descent
   from `renderer.root`) before calling `focus.focus()`, on a 1 ms timeout so the
   remount has settled (`dialog.tsx:100-118`). Naive `savedFocus.focus()` crashes when
   the underlying node was destroyed while the dialog was open.
2. **Escape and ctrl+c both close, but only the top of the stack**, and only when no
   text is selected (`enabled: store.stack.length > 0 && !renderer.getSelection()?.getSelectedText()`,
   `dialog.tsx:120-124`). Selection takes priority over dismissal — a user dragging to
   copy does not lose the dialog.
3. **Backdrop click-to-dismiss is selection-aware**: `onMouseDown` records
   `dismiss = !!renderer.getSelection()`, `onMouseUp` closes only if that flag is
   false (`dialog.tsx:25-33`). This is the fix for "I selected text inside the dialog,
   dragged past the edge, and it closed."

Sizes are a three-value enum (`medium 60 / large 88 / xlarge 116` columns,
`dialog.tsx:21-25`), clamped by `maxWidth: dimensions().width - 2`.

### P9 — Pane focus enum + layout/hit-region cache **[FT-pattern]**

For the multi-pane chat layout (community rail │ channel list │ timeline │ thread),
the FrankenTUI contract describes — and we should independently implement:

- Model pane geometry **explicitly** and map pointer position → focused pane, rather
  than relying on per-widget mouse handlers.
- Cache the layout rectangles computed during render, and read them during event
  handling for hit-testing (FrankenTUI stores them in `Cell<Rect>` during `view()`;
  the concept is "one authoritative rect table per frame", not the Rust cell type).
- Render **visible splitter handles**; support drag-to-resize with clamped bounds;
  represent split ratios in **basis points** (integers, 0–10000) rather than floats to
  keep resize idempotent and snapshot-stable.
- Keep drag state a strict three-phase machine — **Down (arm) → Move (update hover) →
  Up (commit or cancel)** — and clear the latch on any keyboard event.
- **Every mouse drag needs a keyboard peer.** FrankenTUI ships a whole
  `keyboard_drag` module for this. For Buzz the minimum is: pane resize via
  `<leader>` + arrows, and pane focus cycling via `tab` / `shift+tab`.

opencode's layout is much simpler (`routes/session/index.tsx:1165` — a flex row of
`sidebar | scrollbox`) because it has one content pane. Buzz needs three or four, so
this group is where we exceed opencode's shell rather than copy it.

---
## Group 3 — Composer ergonomics

opencode splits the composer into six independent concerns
(`src/prompt/{display,frecency,history,part,stash,traits}.ts` + `src/component/prompt/`).
The 1,713-line `component/prompt/index.tsx` is the assembly; the semantics live in
the small files. Buzz should keep that split.

### P10 — The composer's value is a **parts array**, not a string

`prompt/history.ts:6-25`:

```ts
export type PromptInfo = {
  input: string                                  // the literal typed text
  mode?: "normal" | "shell"
  parts: (FilePart | AgentPart | TextPart & { source?: { text: {start,end,value} } })[]
}
```

The visible text and the structured attachments are **parallel**, joined by
`source.text.{start,end,value}` byte/width ranges. When the user types `@src/foo.rs`,
the textarea shows that literal text, and a `FilePart` records that offsets
`[start,end)` correspond to `file:///abs/src/foo.rs`.

Rendering of those ranges is done with **extmarks** — the editor-buffer primitive for
"a styled, virtual span anchored to a text range" (`autocomplete.tsx:194-200`):

```ts
const extmarkId = input.extmarks.create({
  start: extmarkStart, end: extmarkEnd,
  virtual: true, styleId, typeId: props.promptPartTypeId(),
})
```

Distinct `styleId`s per part type (`fileStyleId` vs `agentStyleId`,
`autocomplete.tsx:192`) so a file mention and an agent mention are visually different
inside the composer.

**Buzz mapping.** Our part types are `channel` (`#general`), `user` (`@alice`),
`agent` (`@claude-1`), `message-ref` (a `buzz://message?...` deep link), and
`file`/`media`. Each gets its own style id and its own colour. The parts array is
what we serialise into the Nostr event's tags — `p` tags for user/agent mentions,
`h`/`e` tags for channel and message refs — so the composer's data model maps
**directly** onto the event we publish. That is a strictly better fit than
opencode's, where parts must be flattened for an LLM.

### P11 — @-mention autocomplete, mechanically

This is the pattern our @-agent-mention mirrors. Verified against
`component/prompt/autocomplete.tsx` (781 lines) and `prompt/display.ts`.

**a) Trigger detection is a pure function over (text, cursor offset).**
`prompt/display.ts:38-47`:

```ts
export function mentionTriggerIndex(value, offset = promptOffsetWidth(value)) {
  const text  = displaySlice(value, 0, offset)      // text up to the cursor
  const index = text.lastIndexOf("@")
  if (index === -1) return
  const before = index === 0 ? undefined : text[index - 1]
  const query  = text.slice(index)
  if ((before === undefined || /\s/.test(before)) && !/\s/.test(query))
    return promptOffsetWidth(text.slice(0, index))  // display-width offset of the '@'
}
```

Three rules, and they are exactly right:
1. Look **backwards from the cursor** for the nearest `@`.
2. The char **before** the `@` must be start-of-input or whitespace (so `foo@bar`,
   i.e. an email, does not trigger).
3. There must be **no whitespace between** the `@` and the cursor (so the popup
   closes once the user types past the token).

Being pure, it is trivially unit-testable — opencode does exactly that in
`test/prompt/display.test.ts`. **Buzz: write the equivalent test first.**

**b) All offsets are display widths, not byte or JS-string indices.**
`display.ts:1-35` builds everything on `Intl.Segmenter(granularity: "grapheme")` plus
`Bun.stringWidth`, with one deliberate correction: *newlines count as width 1*
because the textarea counts them as one position while `stringWidth` counts zero
(`display.ts:7`). `displaySlice`/`displayCharAt`/`promptOffsetWidth` are the only
sanctioned ways to index the composer text. Skipping this is how you get
mis-highlighted mentions the moment someone types CJK or an emoji.

**c) State is four fields.** `autocomplete.tsx:100-105`:

```ts
{ index: 0,             // display offset of the trigger char
  selected: 0,          // highlighted row
  visible: false as false | "@" | "/",   // which trigger opened it
  input: "keyboard" as "keyboard" | "mouse" }
```

`visible` doubles as *is-open* and *which mode*. The `input` field exists to solve a
real TUI bug: when the list re-filters, the layout moves under a stationary mouse
cursor and the terminal emits a synthetic `mousemove`, which would hijack the
selection. The fix is to force `input: "keyboard"` on every filter change
(`autocomplete.tsx:167-170`) and ignore `onMouseOver` unless `store.input === "mouse"`
(`autocomplete.tsx:757-760`). **Copy this.**

**d) The query is read from the editor, then stabilised through an effect.**

```ts
const filter = createMemo(() =>                                  // autocomplete.tsx:146-152
  props.input().getTextRange(store.index + 1, props.input().cursorOffset))
const [search, setSearch] = createSignal("")
createEffect(() => { const next = filter(); setSearch(next ? next : "") })  // :159-162
```

The comment at `:154-157` explains why the extra hop exists: the memo mixes reactive
text with non-reactive cursor state, so mid-keypress it can read a partial value.
Copying it into a signal **from an effect** means all consumers observe the same
settled value after paint. Any front end with a reactive layer hits this; a ratatui
implementation avoids it by reading both in the same `update()` tick.

**e) Candidate sources are separate memos, merged with different ranking rules.**
`autocomplete.tsx:476-525`:

- **Files** come from the server already ranked (`sdk.client.v2.fs.find({query, limit: 20})`,
  `:324-331`) and are **deliberately not re-sorted** by the client fuzzy matcher —
  the comment at `:488-489` notes that re-sorting *loses* results.
- **Non-file options** (agents, reference aliases, MCP resources / for `/`: commands)
  go through `fuzzysort.go` with:
  - `keys`: value-or-display, plus `description` **only in `/` mode** (matching
    descriptions in `@` mode "surfaced unrelated items", `:505-506`),
  - `threshold: 0.5` for `@`, `0` for `/` (`:510`),
  - `limit: 10`,
  - a custom `scoreFn` that **doubles** the score on a prefix match
    (`target.startsWith(trigger + search)`) and then multiplies by `(1 + frecency)`
    (`:512-520`).
- Merge order is `[...fuzziedNonFiles, ...fileOptions].slice(0, 10)` — **local/known
  entities always rank above filesystem results.**
- With an empty query, no fuzzy pass runs at all: `[...nonFileOptions, ...fileOptions]`
  (`:494-496`).
- While the async file resource is in flight, the **previous** list is returned rather
  than an empty one (`if (files.loading && prev?.length) return prev`, `:498-500`) —
  this is what stops the popup flickering as you type.

**f) Agents are a first-class candidate type already.** `autocomplete.tsx:402-421`:

```ts
sync.data.agent.filter(a => !a.hidden && a.mode !== "primary")
  .map(agent => ({ display: "@" + agent.name,
    onSelect: () => insertPart(agent.name, { type: "agent", name: agent.name,
                                             source: {start:0,end:0,value:""} }) }))
```

Note `mode !== "primary"` — the agent you are *already talking to* is excluded from
its own mention list. **Buzz analogue**: exclude yourself from `@`-mentions, and (in
an agent DM) exclude the agent that owns the channel.

**g) Insertion is delete-range-then-insert, then extmark, then part.**
`insertPart` (`autocomplete.tsx:172-240`) in order:
1. Compute whether a trailing space is needed by inspecting the char *after* the
   cursor (`displayCharAt(props.value, cursorOffset) !== " "`, `:176-178`) — no double
   spaces when completing mid-sentence.
2. Convert the display offsets `store.index`..`cursorOffset` into `{row, col}` logical
   cursors by round-tripping through `input.cursorOffset`, then
   `input.deleteRange(...)` + `input.insertText(append)` (`:180-186`).
3. Create the extmark over the inserted span (`:194-200`).
4. Push the part into the prompt store — **de-duplicating** file parts by URL and
   merely updating the existing part's offsets if the same file is mentioned twice
   (`:204-221`).
5. Record frecency for the chosen path (`:237-239`).

**h) Two completion keys with different semantics.** `tab` =
`prompt.autocomplete.complete`, which for a **directory** rewrites the token to
`@path/` and resets selection so you can keep drilling (`expandDirectory`,
`:560-579`); `enter` = `prompt.autocomplete.select`, which commits. `escape` hides.
Directory drill-down via `tab` is the difference between a usable and an annoying
file picker.

**Buzz analogue**: `tab` on a **community** completes to `@community/` and re-queries
that community's members; `tab` on a **channel** with threads could scope to that
channel. `enter` always commits.

**i) Re-opening after backspace.** `onInput` (`autocomplete.tsx:676-708`) does not
only handle "should I close"; when *closed*, it re-runs the trigger detection on
every keystroke, so deleting the space that closed the popup re-opens it with the
right index. It also closes when the cursor moves **before** the trigger
(`cursorOffset <= store.index`) or whitespace appears in the token.

**j) Positioning: absolute box anchored above the composer.**
`autocomplete.tsx:722-732` — `position="absolute"`, `top = anchorY - height`,
`left = anchorX`, `width = anchor.width`, `zIndex: 100`. Height is
`min(10, optionCount, max(1, anchor.y))` (`:712-717`) so the popup never overflows the
top of the screen. Because OpenTUI does not push layout changes reactively, the anchor
is **polled at 50 ms** and a tick signal is bumped only when x/y/width actually change
(`:115-128`). A ratatui implementation gets this for free by recomputing rects each
frame.

**k) A directly-driven insertion path exists too.** `editor.onMention(...)`
(`:664-666` → `insertFileMention`, `:302-314`) lets an *external* source (the editor
integration) insert a mention without going through the popup. Buzz wants the same
seam so that clicking a username in the timeline inserts `@alice` into the composer.

### P12 — Frecency ranking, persisted as JSONL

`prompt/frecency.tsx`. Score is `frequency / (1 + ageInDays)` where
`ageInDays = (now - lastOpen) / 86_400_000` (`:32-35`). Storage is
`<state>/frecency.jsonl` — **append one JSON line per update**, and only rewrite the
whole file when the 1,000-entry cap is exceeded (`:56-73`). On load, later lines win
per key, sorted by `lastOpen`, truncated to the cap (`parseFrecency`, `:14-30`).

This append-mostly JSONL shape is used for all three persisted composer stores
(frecency, history, stash) and is worth adopting wholesale: crash-safe, no locking,
self-healing (corrupt lines are dropped by the `try/catch` in the parse `.map`), and
the rewrite-on-load compacts the file.

**Buzz**: frecency over **people and channels**, not files. `@` ordering should be
"people you actually talk to", which is the single highest-leverage ranking signal in
a chat client.

### P13 — Prompt history with a duplicate guard

`prompt/history.tsx`. `MAX_HISTORY_ENTRIES = 50`; index walks negative from 0 (0 =
"currently typing"); `move()` refuses to move if the user has edited the recalled
entry (`if (current.input !== input && input.length) return`, `:78`) — you cannot lose
edits by pressing up twice. `append()` drops exact duplicates of the previous entry via
`JSON.stringify` comparison (`isDuplicateEntry`, `:44-47`). History stores **whole
`PromptInfo` objects**, so recalling a message restores its attachments too.

### P14 — Draft stash

`prompt/stash.tsx` — `push` / `pop` / `remove(index)` / `list`, cap 50, same JSONL
persistence, `structuredClone(unwrap(...))` before storing so later store mutations
cannot corrupt a stashed entry (`:56`). Bound to `prompt_stash` / `prompt_stash_pop` /
`prompt_stash_list` (`keybind.ts:163-165`, all `"none"` by default — palette-only).

**Buzz needs this more than opencode does**: a chat user typing in `#general` who gets
pulled into `#incidents` must be able to park the draft. Better still, **per-channel
drafts** (the desktop app already has `clearAllDrafts()` in its community-reset list)
plus an explicit stash for cross-channel parking.

### P15 — External editor handoff

`editor_open: keybind("<leader>e", "Open external editor")` → `prompt.editor`. The
`context/editor.ts` (408 lines) module owns spawning `$EDITOR`, and the reverse
channel `editor.onMention` (P11k). **[glamorous-pattern]** The tui-glamorous
production-architecture reference names the same pattern "smart editor dispatch":
suspend the TUI for a terminal editor, but *background* the process for a GUI editor.
Buzz should support `<leader>e` to compose a long message in `$EDITOR` — a genuinely
differentiating feature for a terminal chat client.

---
## Group 4 — Streaming render behaviour

### P16 — Store reconciliation, not append-to-log

`context/sync.tsx` (666 lines) holds one normalised `solid-js/store` keyed by id:

```ts
{ session: Session[], message: { [sessionID]: Message[] }, part: { [messageID]: Part[] },
  session_status: {...}, agent: [], command: [], permission: {...}, ... }
```

SSE deltas are applied with `produce()` (in-place mutation) and `reconcile()`
(structural diff that preserves object identity for unchanged nodes), **not** by
pushing rendered lines into a buffer. Ordered inserts use a **binary search** helper
(`sync.tsx:41-52`) to find the insertion index by key, so out-of-order event arrival
still yields a sorted list in O(log n).

Why this matters for us: fine-grained reactivity then repaints only the cells that
changed, which is the mechanism behind smooth token streaming. **A Nostr event stream
maps onto this 1:1** — and better than opencode's case, because Nostr events are
already immutable, id-addressed records. Buzz's store shape:

```
community[] · channel[] · message{[channelID]: Message[]} · reaction{[eventID]: Reaction[]}
· thread{[rootID]: Message[]} · presence{[pubkey]: Presence} · unread{[channelID]: {count, lastReadTs}}
· agentTurn{[pubkey]: TurnState}
```

Corollaries worth stating explicitly, because they are where naive chat TUIs fail:
- **Idempotent apply.** The same event id delivered twice (reconnect replay) must be a
  no-op. Key everything by event id.
- **Late edits/deletions** (NIP-09 deletes, reactions arriving after their target) must
  reconcile in place, not append a second rendering of the message.
- **Optimistic local echo**: insert the outgoing message with a provisional id, then
  reconcile when the relay echoes the signed event back. Identity-preserving reconcile
  is what stops the message visibly re-mounting.

### P17 — 16 ms batched flush on the event boundary

`context/sdk.tsx:46-77`. Every SSE event goes into a queue; the flush wraps all
pending emissions in a single `batch()` so N events cause **one** render:

```ts
const flush = () => { const events = queue; queue = []; timer = undefined; last = Date.now()
  batch(() => { for (const e of events) emitter.emit("event", e) }) }

const handleEvent = (event) => {
  queue.push(event)
  const elapsed = Date.now() - last
  if (timer) return
  if (elapsed < 16) { timer = setTimeout(flush, 16); return }   // coalesce bursts
  flush()                                                        // but stay instant when idle
}
```

The asymmetry is the point: **an isolated event flushes immediately** (no added
latency on a single keystroke-response), while a burst coalesces to one frame per
16 ms. A fixed debounce would make the idle case feel laggy; a raw pass-through would
thrash the renderer during a token storm.

Reconnect is exponential backoff on the same loop: `1000 * 2^(attempt-1)` capped at
`30_000` ms (`sdk.tsx:47-48`, `:110-112`), with the queue flushed before each retry so
nothing is stranded.

**Buzz**: a busy community can deliver far more events than one agent session — a
relay fan-out burst on reconnect can be thousands of events. Keep the 16 ms coalescing
window and add a **catch-up mode**: when a flush batch exceeds some threshold, suppress
per-message animation and render the settled state once.

### P18 — Sticky-bottom scroll with explicit escape

`routes/session/index.tsx:1181-1182`: `stickyScroll={true} stickyStart="bottom"` on the
message `<scrollbox>`. The container stays pinned to the bottom as content grows, and
un-pins when the user scrolls up. Explicit `toBottom()` uses a 50 ms timeout before
`scroll.scrollTo(scroll.scrollHeight)` (`:417-422`) to let layout settle after a
content change.

Message-granular navigation is built on **child renderable geometry**, not on a line
index (`findNextVisibleMessage`, `:370-399`): filter scrollbox children to those with a
real message id and non-synthetic text parts, sort by `y`, then pick the first with
`y > scrollTop + 10` (next) or the last with `y < scrollTop - 10` (previous), with a
±10 row dead-zone so a partially-visible message is not counted twice. Fall back to a
page scroll when nothing matches (`scrollToMessage`, `:402-415`).

Scroll commands come in four granularities — line / half-page / page / first-last
(`keybind.ts:136-143`), all with `ctrl+alt+*` defaults plus `pageup`/`pagedown`/
`home`/`end`.

**Buzz additions**: jump-to-first-unread (with a persistent "new messages" divider),
jump-to-mention, and jump-to-thread-parent. The unread divider is the one piece of
chat chrome with no opencode equivalent and it must survive re-render, so anchor it to
an **event id**, not a row index.

### P19 — Incremental markdown rendering **[FT-pattern]**

The FrankenTUI showcase contract's streaming-markdown invariants, restated:

- Render **incrementally as fragments arrive**; do not defer to a full re-render at
  completion.
- Show explicit **stream status and progress** (streaming / paused / complete), plus a
  compact progress indicator.
- Provide **stream lifecycle controls** (pause / resume / restart) and focus-aware
  scrolling while streaming.
- Support full GFM: tables, task lists, code blocks with syntax highlighting.

**[glamorous-pattern]** The Charm production-architecture reference adds the
complementary perf rule: **cache rendered markdown by content hash and invalidate on
width change** — re-rendering markdown every frame is the classic TUI jank source.
Buzz should cache per `(eventID, renderedWidth, themeID)`.

OpenTUI ships a `Markdown` renderable (66 KB) and a tree-sitter-backed `Code`
renderable, so on path (b2) this is mostly configuration. On the ratatui hedge it is
the single largest missing component — plan several weeks.

**Buzz specifics**: an agent's streaming reply is a *message being edited in place*.
Do not render each token as a new message. The message row must have a stable identity
from first token to final event, with a visible "typing/working" state that resolves
into the final content.

---

## Group 5 — Command palette and dialogs

### P20 — One dialog primitive, ~30 dialogs

`ui/dialog.tsx` (backdrop + stack + focus restore, P8) and `ui/dialog-select.tsx`
(790 lines — the filterable list) are the only two primitives. Everything else is a
thin config: `dialog-model`, `dialog-session-list`, `dialog-theme-list`,
`dialog-agent`, `dialog-mcp`, `dialog-skill`, `dialog-stash`, `dialog-status`,
`dialog-provider`, `dialog-workspace-*`, plus `dialog-confirm` / `dialog-alert` /
`dialog-prompt` / `dialog-help` in `ui/`.

Several of these are **under 40 lines** (`dialog-agent.tsx` is 729 bytes,
`dialog-theme-list.tsx` is 1.1 KB) — that is the tell that the primitive is right.

The dialog-select keybindings are themselves in the shared table under a dotted
namespace (`keybind.ts:202-209`): `dialog.select.{prev,next,page_up,page_down,home,end,submit}`
with `up,ctrl+p` / `down,ctrl+n` defaults.

**Buzz dialogs**, all on the same primitive: community switcher, channel switcher,
DM/person picker, agent picker, emoji/reaction picker, thread jump, search results,
theme picker, invite, confirm-destructive.

### P21 — The palette is generated from the keymap, not a hand-written list

`component/command-palette.tsx` — 74 lines total, because it is a projection:

```ts
const reachable = keymap.getCommandEntries({ namespace: "palette", visibility: "reachable",
                                             filter: isVisiblePaletteCommand })
const registeredBindings = keymap.getCommandBindings({ visibility: "registered",
                                                       commands: reachable.map(e => e.command.name) })
// → { title, description, category, footer: formatKeyBindings(bindings), value, suggested, onSelect }
```

Three properties fall out:
- **`visibility: "reachable"`** — the palette lists only commands that are *actually
  live in the current mode/layer stack*. A thread-only command does not appear while
  focus is in the channel list. This is a direct benefit of P6.
- Each row's **footer shows its current keybinding**, formatted through the same
  `formatKeyBindings` used by the help dialog, with `<leader>` rendered as the user's
  actual leader key (`keymap.tsx:181-193` `formatOptions`, plus display aliases
  `pageup→pgup`, `meta→alt`). The palette therefore **teaches the keybindings**.
- The palette **excludes itself** (`command.name !== COMMAND_PALETTE_COMMAND`) and
  respects a per-command `hidden` flag.

Slash commands are the same projection through a different lens (`useCommandSlashes`,
`keymap.tsx:262-290`): any command carrying a `slashName` becomes `/name` with
aliases, dispatched through `keymap.dispatchCommand`. **One definition, three
surfaces** (key / palette / slash) — this is the payoff of P1+P3 and the thing to
insist on in Buzz's design review.

### P22 — Fuzzy + "Suggested" + categories

`command-palette.tsx:62-72`: with no filter text, a **Suggested** section is prepended
(entries whose `suggested` is `true`, or a predicate evaluating true — so "suggested"
can be *contextual*: e.g. "Mark all read" only when unread > 0). Once the user types,
the suggested block disappears and the plain fuzzy-ranked list is shown. Rows carry a
`category` for section headers.

**[FT-pattern]** The FrankenTUI search invariants add requirements opencode's palette
does not meet, which we should apply to Buzz's **message search** (a first-class
chat surface, unlike opencode):
- search-as-you-type, not submit-only;
- explicit focus entry/exit (`/` or `ctrl+f`, `escape`);
- fast match navigation (`n`/`N`, `enter`/`tab`);
- **match count visibility** (`current/total`) in the search bar *and* status bar;
- three-level match hierarchy: list-level gutter marker, line-level highlight,
  stronger emphasis on the *active* match;
- contextual affordances: nearest-context snippet, match-density indicator.

**[glamorous-pattern]** Debounce the search input ~150 ms and fire only when typing
stops; flatten all searchable fields into one composite string for zero-allocation
fuzzy matching. For Buzz, the relay does the real search (NIP-50 → `buzz-search`
Postgres FTS via `POST /query`), so the debounce protects the **relay**, not just the
render loop — and the CLI gotcha applies: **a search query must always specify
`kinds`** or it hits the relay p-gate and 403s.

---

## Group 6 — Theming

### P23 — A closed semantic token set, with the syntax palette *derived*

`theme/index.ts` (1,089 lines) defines a `Theme` type with ~55 readonly `RGBA` tokens
in five families:

| Family | Tokens |
|---|---|
| Semantic | `primary secondary accent error warning success info` |
| Text | `text textMuted selectedListItemText` |
| Surface | `background backgroundPanel backgroundElement backgroundMenu` |
| Border | `border borderActive borderSubtle` |
| Domain | `diff*` (13 tokens), `markdown*` (14), `syntax*` (9), `thinkingOpacity` |

34 themes ship as JSON assets (catppuccin, gruvbox, tokyonight, nord, rosepine,
dracula, everforest, …). Each theme entry is `{dark, light}` (`theme/index.ts:116-117`),
so **light/dark are one theme, not two**, and the app honours the terminal's reported
mode with an explicit lock (`theme_switch_mode`, `theme_mode_lock` in the keybind
table).

The crucial move: **syntax and system palettes are generated from the base theme**
(`generateSyntax`, `generateSubtleSyntax`, `generateSystem`, `tint`) rather than
hand-specified per theme. A theme author supplies a dozen colours; the other forty are
derived. That is why 34 themes are maintainable.

Selection foreground is computed, not stored: `selectedForeground(theme)` +
`_hasSelectedListItemText` (`theme/index.ts:89`) so that themes which do not specify
one get a contrast-correct value.

**[FT-pattern]** FrankenTUI's colour-harmony invariants add the discipline layer:
- Prefer **curated tokens over ad-hoc colours** — no literal hex in feature code.
- **Neutral tones for large surfaces and chrome; high-chroma accents reserved for
  focus/selection.** This single rule is most of the difference between a TUI that
  looks designed and one that looks like a Christmas tree.
- Semantic colour mappings must be **intentional** (status, priority, per-screen
  accent), and **contrast is enforced by tests** (a WCAG contrast test suite over the
  token set). Adopt the test: a Buzz CI check that asserts every
  `(foreground, background)` pair in the token table clears WCAG AA, per theme, in
  both modes.
- Theme cycling must be **globally reachable** from any screen and also exposed as a
  palette command.

**[glamorous-pattern]** Honour `NO_COLOR` and `TERM=dumb`; test against both light and
dark terminal backgrounds; pre-compute styles once at startup rather than per frame.

**Buzz specifics**: per-community accent colour (so you can tell at a glance which
community you are in — the tab-strip-as-identity idea from the FrankenTUI global-shell
contract), and deterministic per-user colours derived from the pubkey (hash → hue,
clamped to a theme-appropriate lightness/chroma band so it stays legible in both
modes).

---

## Group 7 — Attention and notifications

### P24 — Focus-aware notification with explicit skip reasons

`attention.ts` (260 lines) is close to directly applicable and is the single most
underrated file in opencode's TUI.

- Terminal focus is tracked as a **three-state** `"unknown" | "focused" | "blurred"`,
  driven by renderer `focus`/`blur` events (`:118-127`). "Unknown" is a real state —
  terminals that do not report focus must not be treated as blurred.
- Each notification declares **when** it applies: `TuiAttentionWhen = "always" |
  "focused" | "blurred"`, resolved by `focusSkip()` (`:107-112`). Default for a
  *desktop notification* is `"blurred"` (don't pop a toast at someone staring at the
  screen); default for a *sound* is `"always"`.
- `notify()` never throws and always returns a **structured result**:
  `{ok, notification, sound, skipped?: reason}` where reason ∈ `attention_disabled |
  renderer_destroyed | empty_message | focus_unknown | focused | blurred`
  (`:60-68`, `:172-215`). Debuggable-by-design: you can always answer "why didn't I get
  pinged?"
- Message text is **sanitised** before it reaches the OS: `stripAnsi`, newlines
  collapsed to spaces, control characters removed, then truncated by **grapheme count**
  (`Array.from(...).slice(0, limit)`) — title 80, message 240 (`:70-77`).
- Sounds are a **pack** system with per-event names (`default question permission error
  done subagent_done`) and a three-level fallback chain: user config → active pack →
  builtin (`soundCandidates`, `:145-149`). Packs are registerable by plugins, the active
  pack persists in KV, volume is clamped to `[0,1]`.

**Buzz event classes** (replacing opencode's): `dm` · `mention` (@you) · `agent_done` ·
`agent_needs_input` · `channel_message` (muted by default) · `error` ·
`connection_lost`. Per-channel and per-community mute state, plus a global DND, layer
on top of the same `when` mechanism. Anything that would fire while the terminal is
focused *and* the relevant channel is on screen should be suppressed by default —
that is the chat-specific instance of the `focused` skip reason.

---
## Group 8 — Space-constrained resilience

### P25 — Responsive tiers, tiny fallback, graceful degradation **[FT-pattern]**

FrankenTUI treats this as an **always-required** invariant (not capability-gated), and
it is the right call for a chat client that people run in a 40-column split pane.
Restated:

- Use **explicit layout tiers** (XS/SM/MD/LG/XL breakpoints) with a distinct **tiny
  fallback layout**, rather than letting a flex solver squeeze panes into
  illegibility.
- **Gate optional subpanels by available area** so core controls stay visible. The
  priority order must be declared, not emergent.
- **Guard empty areas**: rendering into a zero-size rect must be a no-op, not a panic
  or a divide-by-zero.
- **Add tests that assert small-size behaviour** — FrankenTUI has explicit
  threshold tests and small-size render tests per screen. This is the part teams skip
  and then regress.
- Per-breakpoint value resolution (padding, label vs icon, abbreviated vs full
  timestamps) rather than one-off `if (width < 80)` checks scattered through views.

**Buzz degradation ladder** (proposed, by terminal width):

| Tier | Cols | Layout |
|---|---|---|
| XL | ≥ 160 | community rail + channel list + timeline + thread pane |
| LG | ≥ 120 | channel list + timeline + thread pane |
| MD | ≥ 90 | channel list + timeline; thread opens as an overlay |
| SM | ≥ 60 | timeline only; channel list is a dialog (`<leader>k`) |
| XS | < 60 | timeline + composer only; all navigation via palette/dialogs |

Minimum supported: **80×24** per the tui-glamorous pre-flight checklist; XS exists so
40-column panes degrade rather than break.

**[glamorous-pattern]** The rest of that pre-flight checklist applies verbatim:
handle resize events and re-layout every component; handle `ctrl+c` with terminal-state
cleanup; detect piped stdin/stdout and fall back to plain text; provide a
`--no-tui`/`NO_TUI` escape hatch; test with `NO_COLOR=1` and `TERM=dumb`.

**[FT-pattern]** Two more disciplines from FrankenTUI's architecture notes that a
Rust implementation should adopt (and that a TS implementation should emulate):
- **One-writer rule** — a single owner of all stdout writes. Every "my TUI is
  garbled" bug traces to two writers.
- **RAII terminal cleanup** — terminal state is restored even on panic. In TS, the
  equivalent is a process-level handler that restores the terminal on uncaught
  exception, `SIGINT`, and `SIGTERM`.
- **Inline mode** (stable chrome at top/bottom while content scrolls in native
  scrollback) is worth evaluating for Buzz specifically: chat users want to select and
  copy history with the mouse and keep it in their scrollback. Very few stacks do this
  well. OpenTUI does not; note it as an open question rather than a plan.
- **Bounded ring buffers** (`VecDeque`-style) for message history in memory, with the
  full history paged from the daemon on demand.

---

## Group 9 — Connectivity (Buzz-original)

### P26 — Reconnect state is chrome, not a toast

No source pattern covers this: opencode's daemon is on localhost, so its connection
model is "it's there or the app is broken". Buzz's daemon may be remote (VPS), and the
daemon's own relay connection may drop independently. Two links, either can fail:

```
TUI ──HTTP+SSE──▶ buzz-daemon ──WebSocket/NIP-42──▶ relay
```

Requirements:

- The status bar must show a **three-state** indicator per link: connected /
  reconnecting (with attempt count and next-retry countdown) / failed. Not a
  transient toast — an unread-looking chat that is actually a dead socket is the
  worst failure mode in this app.
- Reuse the **exponential backoff** shape from P17 (`1s → 30s` cap) on both links, and
  surface the backoff timer rather than hiding it.
- **Send while disconnected** must queue locally and show a per-message pending state,
  with an explicit retry/discard affordance — never a silent drop.
- On reconnect, the catch-up burst goes through the P17 batching path, and the
  **unread divider anchors to the last-read event id**, so a reconnect never loses the
  user's place.
- **Auth failures are distinct from network failures** (NIP-42 rejection, expired
  auth tag) and must say so, with a specific remediation, not "disconnected".

---

## Anti-patterns (collected)

Recurring failure modes named across the three sources, worth putting in the design
review checklist:

1. **Boolean focus flags** instead of a mode stack (P6) — leads to keys firing in the
   composer.
2. **Byte or JS-string offsets** in the composer instead of display widths (P11b) —
   breaks on emoji/CJK.
3. **Append-to-log rendering** instead of store reconcile (P16) — breaks edits,
   deletes, reactions, and duplicate delivery.
4. **Raw event pass-through** to the renderer with no coalescing (P17) — thrashes on
   bursts; and its opposite, a fixed debounce, which adds latency when idle.
5. **Re-rendering markdown every frame** instead of caching by content hash (P19).
6. **Hardcoded keys in handlers** rather than one declarative table (P1) — the palette
   and help immediately drift from reality.
7. **Hand-written palette lists** (P21) — drift again, plus commands that appear while
   unreachable.
8. **Ad-hoc hex colours** in feature code instead of semantic tokens (P23).
9. **Notifications with no skip reasons** (P24) — undebuggable "why no ping".
10. **Layout that assumes ≥ 100 columns** with no tiny fallback and no small-size
    tests (P25).
11. **Mouse-only affordances** with no keyboard peer (P9) — drag-resize, link
    activation, reaction picking.
12. **Comparing screenshots without retaining run metadata** (from tui-inspector's
    anti-pattern list) — a failed capture that produced a valid-looking MP4 is not a
    pass.

---
## Dogfood and test pipeline for the Buzz TUI

Derived from `~/.claude/skills/tui-inspector/SKILL.md` (VHS + deterministic snapshot
methodology, `/dp/tui_inspector/scripts/*`), FrankenTUI's determinism tooling
**[FT-pattern]**, and opencode's own test layout (`packages/tui/test/`).

Tooling present on this box, verified 2026-08-04: `vhs` (`~/go/bin/vhs`), `ttyd`
(`/usr/bin/ttyd`), `ffmpeg` (`/usr/bin/ffmpeg`), `tmux`. Missing: `asciinema`, `agg`
(not needed for this design).

### Four tiers, cheapest first

```
T0  pure unit          ms      no terminal    every commit, every push
T1  headless render    ~1 s    fake terminal  every commit  (the CI workhorse)
T2  tmux drive         ~5 s    real PTY       PR gate
T3  VHS record         ~30 s   real PTY+video PR artifact + release, never a gate
```

The rule that keeps this affordable: **assert on text, record video only for humans.**
Video is evidence, not a test oracle.

---

### T0 — Pure unit tests over the pattern primitives

Everything in the pattern list that is a **pure function over data** must be unit
tested with no terminal at all. opencode does exactly this — `test/prompt/display.test.ts`,
`history.test.ts`, `part.test.ts`, `persistence.test.ts`, `jsonl.test.ts`,
`test/keymap.test.tsx`, `test/theme.test.ts`.

Buzz's T0 set:

| Target | Asserts |
|---|---|
| `mentionTriggerIndex` (P11a) | triggers at start-of-input and after whitespace; **not** in `foo@bar`; closes on whitespace in token; correct display offset with emoji/CJK before the `@` |
| display-width helpers (P11b) | grapheme clusters, ZWJ emoji, CJK double-width, newline-counts-as-1 |
| candidate ranking (P11e) | prefix match doubles score; frecency multiplier; local entities rank above remote results; empty query skips fuzzy |
| part/extmark offsets (P10) | insert mid-sentence produces one space not two; duplicate mention updates existing part rather than appending |
| frecency (P12) | `freq/(1+ageDays)`; cap eviction; corrupt JSONL line dropped not fatal |
| history (P13) | duplicate guard; edited-entry move refusal; index clamping |
| keybind parse (P1/P4) | `"none"`/`false` unbind; comma multi-bind; unknown key → error naming the offender; alias expansion |
| store reconcile (P16) | duplicate event id is a no-op; out-of-order insert lands sorted; delete/reaction reconciles in place |
| theme contrast (P23) | every token pair clears WCAG AA, per theme, both modes |

Gate: `just tui-test-unit`, wired into the existing pre-push hook alongside the other
fast unit suites.

---

### T1 — Headless render snapshots (the CI workhorse)

Render the component tree to an **in-memory cell buffer** at fixed dimensions and
snapshot the **text grid**, not pixels.

- On path (b2) OpenTUI/TS: render at a fixed size and serialise the buffer to lines.
- On path (a) ratatui: `TestBackend` + `Buffer` is exactly this and is the reason the
  hedge is attractive for testing.

**[FT-pattern]** FrankenTUI's determinism model is the target contract: a
**deterministic render pipeline** (buffer → diff → presenter, no hidden I/O), a
**shadow-run comparison** that asserts two runs of the same input produce identical
buffers, and **per-screen snapshot tests with a `BLESS=1` update mode**. Adopt all
three ideas; implement them ourselves.

Determinism requirements — a snapshot suite is worthless without these:

1. **Frozen clock.** All timestamps from an injected clock; `BUZZ_TUI_FIXED_TIME`.
2. **Frozen randomness.** Seeded PRNG for anything sampling (spinner phase, jitter).
   FrankenTUI uses a seed-driven LCG for exactly this.
3. **No animation.** `BUZZ_TUI_NO_ANIM=1` pins spinners/pulses to frame 0. opencode
   already has an `app_toggle_animations` command — make it an env var too.
4. **Fixed dimensions** per snapshot, declared in the test name.
5. **Fixed theme**, fixed locale, `TZ=UTC`.
6. **Fixture-backed daemon.** T1 never touches a network. See "Fixture protocol".

Snapshot matrix — every screen × every breakpoint tier (P25):

```
snapshots/<screen>/<tier>.txt      screens: channel-list, timeline, thread, dm,
                                            composer-empty, composer-mention-open,
                                            palette, help, search-results,
                                            community-switcher, unread-divider,
                                            agent-streaming, disconnected, auth-failed
                                   tiers:   xs(50x20) sm(70x24) md(100x30)
                                            lg(140x40) xl(180x50)
```

The `xs`/`sm` rows are the small-size tests FrankenTUI insists on and everyone skips.
`disconnected` and `auth-failed` cover P26; `unread-divider` and `agent-streaming` are
the chat-specific states with no upstream analogue.

Update flow: `BLESS=1 just tui-test-render` rewrites snapshots; the diff is reviewed
in the PR like any other diff.

Gate: **required** on every PR. Runs in seconds, no PTY, no video.

---

### Fixture protocol — the thing that makes T1 and T2 deterministic

The TUI's only coupling to the backend is a URL + headers (opencode's
`runTui({url, headers})`, `packages/cli/src/tui.ts:7`). We inherit that, and it is what
makes the whole pipeline testable:

```
BUZZ_TUI_FIXTURE=/path/to/scenario.jsonl   → in-process fake transport
BUZZ_DAEMON_URL=http://127.0.0.1:PORT      → real daemon
```

A scenario file is an ordered JSONL of `{atMs, kind, payload}` records replayed on the
frozen clock: initial state snapshot, then the event stream. Scenarios to author first:

| Scenario | Exercises |
|---|---|
| `empty` | cold start, no communities — the empty-state baseline (tui-inspector's `analytics-empty` equivalent) |
| `seeded-basic` | 3 channels, ~40 messages, 2 unread |
| `mention-burst` | 200 events in 100 ms → P17 coalescing + catch-up mode |
| `agent-stream` | one agent reply arriving token-by-token → P19 in-place message identity |
| `edit-delete-react` | NIP-09 delete + late reaction → P16 reconcile-in-place |
| `reconnect` | SSE drop, backoff, replay of already-seen ids → idempotent apply + unread divider survival |
| `auth-fail` | NIP-42 rejection → P26 distinct error surface |

The same fixture files drive T1, T2, and T3. That single-source property is what keeps
the tiers from disagreeing.

---

### T2 — tmux capture-pane driving (the dogfood loop)

For interaction sequences and for a human to actually *use* the thing. `tmux` is
better than VHS as a **gate** because it is fast, headless, and yields text.

```bash
# 1. launch into a detached session at a fixed size
tmux new-session -d -s buzztui -x 120 -y 40 \
  "BUZZ_TUI_FIXTURE=$FIX BUZZ_TUI_NO_ANIM=1 TZ=UTC ./buzz-tui"

# 2. wait for a READY marker rather than sleeping blind
until tmux capture-pane -p -t buzztui | grep -q "#general"; do sleep 0.1; done

# 3. drive
tmux send-keys -t buzztui 'C-p'            # command palette
tmux send-keys -t buzztui 'theme'          # filter
tmux capture-pane -p -t buzztui > /tmp/palette.txt

# 4. assert on text
grep -q "Switch theme" /tmp/palette.txt

# 5. always clean up
tmux kill-session -t buzztui
```

Rules learned from the tui-inspector methodology, restated for tmux:

- **Never `sleep N` as a readiness proxy** — poll `capture-pane` for a known marker
  with a hard timeout. Blind sleeps are the #1 source of flaky TUI CI.
- **Fixed pane geometry** (`-x`/`-y`), set at creation, never resized mid-test unless
  resize is what you are testing.
- **Isolated runtime state per run** — a per-run `XDG_STATE_HOME` so the JSONL stores
  (frecency/history/stash, P12–P14) start empty and cannot be corrupted by a parallel
  run. This mirrors tui-inspector's per-run `STORAGE_ROOT` + isolated `DATABASE_URL`
  discipline, which exists precisely to avoid flaky captures from shared local state.
- **`capture-pane -p`** for the visible screen; `-p -S -` for full scrollback when
  testing that history is preserved (relevant to the inline-mode question in P25).
- **`capture-pane -e`** preserves escape sequences when the assertion is about
  *colour* (e.g. "the unread channel is rendered in the accent token").
- **Always kill the session in a trap** — orphaned tmux sessions on the VPS are how
  this suite becomes a resource leak.

T2 sequences worth gating (each an interaction, not a still):

| Sequence | Asserts |
|---|---|
| type `@ali` in composer | popup opens, `@alice` ranked first, extmark styling applied |
| `tab` on a community mention | drills into `@community/`, re-queries members |
| `<leader>` then wait | which-key panel appears listing chat verbs (P5) |
| `<leader>` then `backspace` | pending sequence pops, panel closes (P2) |
| `ctrl+p` → type → `enter` | palette filters, dispatches, closes, **focus returns to composer** (P8) |
| open dialog → `escape` | closes top of stack only; focus restored (P8) |
| scroll up during a stream | sticky-bottom releases; new messages do not yank the viewport (P18) |
| jump-to-first-unread | divider anchored to event id, survives a subsequent burst |
| resize 120→60→120 | tier switch and back, no panic, no lost state (P25) |
| kill fixture transport mid-run | status bar shows reconnecting + countdown (P26) |

Gate: **required** on PRs that touch `buzz-tui`. Runs on the standard runner; no video
dependencies.

---

### T3 — VHS recording (evidence, not oracle)

Use `vhs` for PR artifacts, release notes, and docs. Adapt the tui-inspector script
shape (`/dp/tui_inspector/scripts/capture_mcp_agent_mail_tui.sh`) rather than its
mcp-agent-mail-specific profiles.

Tape shape it generates, worth copying:

```
Output "capture.mp4"
Set Shell "bash"
Set FontSize 18
Set Width 1600
Set Height 960
Set TypingSpeed 0ms          # deterministic: no simulated human typing delay
Type "cd <dir> && <binary>" Enter
Sleep <boot>s
<key script>
Sleep <capture>s
Hide
Ctrl+C
Show
Sleep 300ms
```

Two details that matter: `Set TypingSpeed 0ms` (typing delay is a determinism hazard),
and the `Hide / Ctrl+C / Show` trailer so the quit keystroke does not appear in the
recording.

Per-run artifact contract, adopted directly from tui-inspector:

```
artifacts/tui/<run-name>/
  capture.tape        exact tape used  — required for reproduction
  vhs.log             stderr
  capture.mp4
  snapshot.png        frame extracted at --snapshot-second via ffmpeg
  run_meta.json       machine-readable outcome
  fixture.jsonl       the scenario that was replayed
```

`run_meta.json` is the canonical outcome, with the fields tui-inspector defines:
`status`, `duration_seconds`, `vhs_exit_code`, `seed_exit_code`, `snapshot_status`,
`video_exists`, `snapshot_exists`, `video_duration_seconds`. Plus Buzz additions:
`fixture`, `tier`, `commit`, `tui_version`.

**Strictness flags are mandatory in CI.** The tui-inspector anti-pattern list is
explicit: a passing MP4 generation is **not** success when the seed step failed. Our
equivalents: fail the run if the fixture did not load, and fail if snapshot extraction
did not produce a PNG.

Suite mode for release: run every scenario × the `md` tier, emit a manifest and an
`index.html` contact sheet, tar the directory, attach to the release. Never gate a PR
on video.

**Screenshot-distinctness gate.** The desktop app's CLAUDE.md already learned this the
hard way and it applies verbatim here: when several captures render similar screens,
hash them and require uniqueness before posting.

```bash
shasum -a 256 artifacts/tui/*/snapshot.png | awk '{print $1}' | sort | uniq -d
# any output = two captures took the same picture = fix the tape, do not post
```

**PR posting.** Reuse `scripts/post-screenshots.sh` (per-developer
`agent-screenshots/<username>` branch, commit-SHA-based immutable URLs). Do **not**
use `buzz upload` or relay media URLs for PR images — they fail through GitHub's camo
proxy. Run `scripts/check-pr-image-urls.sh` on any hand-edited PR markdown. Delete
superseded screenshot comments after reposting.

---

### CI wiring

```
pre-commit   fmt + lint (biome/rustfmt)                            seconds
pre-push     T0 unit + T1 render snapshots                         < 30 s
PR (required)  T0 + T1 + T2 tmux sequences                         ~2 min
PR (artifact)  T3 VHS suite → upload, comment via post-screenshots.sh
release        T3 full suite × all tiers → tarball + contact sheet
nightly        T2 full matrix + T3 full suite + fixture drift check
```

**Fixture drift check** (nightly): replay each scenario against a **real** daemon +
ephemeral relay and diff the resulting event stream against the recorded fixture. This
is the only thing standing between a green fixture-based suite and a TUI that no longer
matches the actual protocol. Failures here are a fixture-regeneration task, not a TUI
bug.

### Incident workflow (adapted from tui-inspector)

1. Reproduce at T1 first — it is seconds and yields a text diff.
2. If T1 is clean, reproduce at T2 with the same fixture; capture the pane text at each
   step.
3. Only then go to T3 for a video, and inspect `run_meta.json` **before** watching it —
   a failed fixture load looks like a product bug on screen.
4. Attach `capture.tape` + `fixture.jsonl` + `run_meta.json` to the issue. A video with
   no tape and no metadata is not a reproduction.

---

## Open questions

- **Inline mode** (stable chrome + native scrollback, P25) is highly desirable for a
  chat client and OpenTUI does not appear to support it. Determine whether path (b2)
  can do it at all; if not, that is a real point for the ratatui hedge.
- **`@opentui/ssh`** would let us serve the Buzz TUI over SSH — a strong fit for a
  remote-first agent platform, and it would make T2/T3 trivially remote. Worth a spike.
- **Which reactive-layer artefacts survive a ratatui port?** P11d (the settle-through-
  an-effect hop) and P11j (50 ms anchor polling) are OpenTUI-specific workarounds that
  a frame-loop architecture does not need. Keep them out of the *spec* so the spec
  stays portable.
- **Per-channel drafts vs a single stash** (P14) — the desktop app already has
  per-channel drafts; the TUI should probably match rather than copy opencode's single
  stash.
