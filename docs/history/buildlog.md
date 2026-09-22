# Ranch — milestone build log

Dated completion notes for shipped features. Append-only.

## 2026-09-20 — Agent ask-user tool (`ranch_ask`) + mobile notifications & settings

**Agent question tool (`ranch_ask`)** — a sixth agent tool so agents can
block on a human decision:

- Protocol: `AgentAsk → AgentAskOk {ask_id}`, broadcast
  `AgentAskRequest`, `AgentAskAnswer` (any client, first-wins,
  re-broadcast), `AgentAskStatus/Ok` (agent poll). 30-min TTL →
  "no answer". Frames documented in PROTOCOL.md §8.1.
- Daemon: `agenttools.rs` gains `AskRegistry`/`AskRecord` (+unit tests);
  `daemon.rs` handles ask/answer/status and prunes resolved asks in the
  tick loop. Answering is first-wins and idempotent.
- Control API: `POST /agent/ask` + `POST /agent/ask/status`.
- Pi extension (`tools/ranch-pi-ext/index.js`): `ranch_ask` tool —
  `question`, `choices[]`, `suggested`, `multi` (select-many),
  `free_text`; blocks by polling `ask/status` until answered or 30 min.
- Surfaces: TUI prompt-line question + `:answer <n|text>` command
  (bare `:answer` = suggested choice); web inline question card above
  the chat input (choice buttons, suggested badge, free-text, Send);
  mobile question card in the terminal screen.
- Forge bridge path (remote forge agents) is a follow-on (F3a-style),
  not shipped.

**Mobile notifications + settings page**:

- New `mobile/screens/Settings.tsx` (⚙ tab) with four toggles:
  turn-end, every message, ignore tool calls, agent questions —
  persisted in AsyncStorage (`ranch.settings.v1`), plus a permission
  row and test-notification button.
- New `mobile/lib/notifications.ts`: expo-notifications 57 channel
  "ranch", background-only delivery (no banners while the app is the
  active screen). Turn-end fires on working→idle `meta kind=agent`;
  per-message on new assistant rows; tool calls notify only when
  "ignore tool calls" is off; questions notify on `AgentAskRequest`.

Verified: `cargo test` green (protocol round-trips + ask registry),
scratch-daemon e2e of the full ask→answer→status flow PASS,
`mobile` + `web` `tsc --noEmit` clean.

## 2026-09-21 — Agent status machine-wide + native Android client (`mobile-native/`)

**Agent `Meta` broadcast fix** (`bf045b8`) — the daemon only routed
`Meta`/`Chat` frames to clients *attached* to a session, so a phone on the
sessions list (`attach=None`) received no agent frames and fired no
notifications. Agent working/idle is now machine-level lifecycle info and is
broadcast to **all** connected clients (like `AgentAskRequest`); model/
context stay attach-scoped. This is what makes parallel-session background
notifications possible on any client.

**Native Android client** (`mobile-native/`, v0.2.0) — a Kotlin app that
replaces the RN client's weak background-notification path. Its primary job
is a foreground service that holds the Supabase Realtime socket (exempt from
Doze) so agent-event notifications fire while the phone is in a drawer.
- Hand-rolled Phoenix client over the private channel
  `realtime:machines:{id}` (JWT in join payload) — port of `relay.rs`:
  25 s heartbeats, 75 s zombie guard, JWT refresh ~2 min pre-expiry,
  chunk reassembly. No daemon/protocol changes; reuses the existing
  machine-wide broadcast.
- Notification engine port of `notifyEvents.ts` (turn-end / every-message /
  question, background-only, replay-safe dedup, 4 per-event toggles).
- Foreground service restores the machine from Prefs on process kill
  (START_STICKY). `applicationId` `dev.ranch.android` coexists with the RN
  app (`dev.ranch.app`) during migration.

Verified: `./gradlew assembleRelease` clean (AGP 8.12.0 / Gradle 9.4.1 /
Kotlin 2.2.10, SDK 35), APK signed with the shared `ranch` keystore, all
classes present in the DEX. APK at `releases/ranch-native-0.2.0.apk`.

Follow-ons: terminal-screen rendering (Phase 2), Google-OAuth login path
(email/password is the current path), FCM as an optional true-push channel.

## 2026-09-21 — Agent `Meta` broadcast root-cause fix + native joinOk

**Root cause (daemon):** the local-pi harness's `write_status()` emitted
`Frame::Meta { kind:"agent", pane: None }` for every working/idle transition.
The daemon's `Meta` handler resolves the owning session *from the pane UUID*;
with `pane: None` the lookup fails and the frame is dropped **before** the
machine-wide broadcast — so no client (TUI, relay, native) ever saw agent
working/idle. The `all = kind=="agent"` recipient fix (`bf045b8`) was
necessary but not sufficient: the frame never reached the broadcast step.
`write_status()` now carries the pane UUID (`pilocal.rs`), matching
`write_model_status`/`write_context_status`, which already worked. Forge
agent Meta already carried the pane, so only the local-pi path was broken.

**Root cause (native client):** `Realtime.kt` checked the join reply at
`payload.reply.status`; Supabase returns it at `payload.status`, so `joinOk`
never latched and the pump skipped all heartbeats/refresh. Now reads
`payload.status`.

Verified end-to-end against the live relay: spawned a scratch pi agent, and a
standalone WS client on `realtime:machines:{id}` (machine JWT, matching the
native client) received the `Meta agent working → idle` edge and the daemon
logged `relay: broadcasting frame`. Test sessions cleaned up.

APK rebuilt (`releases/ranch-native-0.2.0.apk`) with the joinOk fix.

## 2026-09-21 — Native Phase 2: session list + terminal/chat screen

The native app is now actually *usable*, not just a notification daemon.

- **Outbound frames** (`Realtime.kt`): client→daemon sends over the shared
  machine channel as Realtime `broadcast {event:"frame", payload:<frame>}`,
  mirroring the daemon's relay receive side. Frames sent before the channel
  is joined are queued (bounded `ConcurrentLinkedQueue`) and flushed on
  joinOk, so `Hello→Attach→Resize→Input` stay in order. A `Hello` is sent
  automatically on (re)join.
- **Frame bus** (`Monitor.kt`): `RelaySession` fans out every frame to
  registered sinks; the Notify engine always runs, and the active
  `SessionActivity` registers/unregisters itself. The live session list is
  tracked from `HelloOk` + `SessionsAck` + `Meta exited`.
- **Session list** (`MainActivity`): a live list of sessions (tapping opens
  the session screen) and a "+ New shell session" button.
- **Session/terminal screen** (`SessionActivity.kt` + `TerminalView.kt` +
  `Sgr.kt`): attaches to a session and renders its active pane.
  - PTY panes → `TerminalView` paints SGR-tagged rows into a monospace cell
    grid (per-cell fg/bg, bold/underline, blinking cursor block).
  - Agent panes (`kind=forge-chat`) → chat list (role labels, collapsed tool
    calls, model line) + `ChatSend` box.
  - PTY input: sentinel-space `EditText` (backspace→DEL, newlines→CR) + a
    special-key row emitting the same byte sequences as the RN client.
  - Geometry: the view computes cols/rows from pixel size and sends
    `Resize`; multi-pane sessions get a tab row (`PaneSelect`).
  - Resync: per-pane `seq` gap → re-`Attach`.

Verified against the live daemon: local-socket `Hello→Attach→Snapshot→
Resize→Input(echo)→Update` (PTY path) and Realtime `join→Hello→HelloOk→
Attach→chunked Snapshot` (chat path, including chunk reassembly). Bumped to
v0.3.0 / versionCode 2 → `releases/ranch-native-0.3.0.apk`.

Phase 3 follow-ons: terminal polish (predictive echo, scrollback paging,
CJK/wide-char metrics, true split layout), model picker, file editor,
workflows/triggers/machines, Google-OAuth login.

---

## Native app — Phase 3: terminal + chat feel (2026-09-21)

`mobile-native/` v0.4.0 (versionCode 3) — `releases/ranch-native-0.4.0.apk`.

Turns "it works" into "I actually use it." All of 3a+3b landed except
split-pane layout / CJK metrics / on-device IME tuning (deferred):

**3a — terminal feel (PTY panes):**
- **Predictive echo**: on each keystroke the typed chars paint dimmed at
  the last-known cursor in `TerminalView` (`setPrediction`) and clear when
  the authoritative `Update` for that row lands (`applyUpdate` resets the
  prediction). Makes typing over the relay feel instant.
- **PTY scrollback**: a `hist` key sends `ScrollbackReq{offset:0,limit:2000}`
  → `Scrollback{lines}` rendered in a popup of monospace text.

**3b — agent chat (forge-chat panes):**
- **Markdown**: `markdownToSpannable` renders `**bold**`/`*italic*`/
  `` `code` ``/headers/lists into a `SpannableStringBuilder` (no WebView).
- **Chat scrollback paging**: scrolling the chat to the top fires
  `ChatHistory{before,limit}` → `ChatHistoryOk`; older rows prepend,
  `has_more` re-arms. `chat_has_more` is inferred from the initial tail
  length (`>= CHAT_TAIL`).
- **Model picker + context readout**: a model chip above the chat shows the
  pane's `model` + `context` (`Meta model`/`context`). Tapping it sends
  `ModelList` → `ModelListOk` (catalog) and renders a `PopupWindow` list;
  a tap sends `ModelSet` and updates the chip.
- **Agent question card**: `AgentAskRequest` renders an interactive card
  (choice rows w/ suggested marker, multi-select ◉/○, free-text box, Send)
  pinned above the input. It mutates buttons in place on toggle (so the
  typed free-text is not wiped on re-render). Send posts
  `AgentAskAnswer{choices,text}`; a broadcast `AgentAskAnswer` (from any
  client) dismisses the card and shows an "answered:" note. This is the
  phone UI for the `ranch_ask` agent tool.

**New frame builders** in `Term.kt`: `agentAskAnswer`, `modelList`,
`modelSet`, `chatHistory`, `scrollbackReq`, `sessionsKill`, `sessionsRename`,
`upgrade` + `parseAgentAsk`/`parseModelChoice` + `AgentAsk`/`ModelChoice`.

**Verification** (live daemon, local socket + chunk reassembly):
`ModelList→ModelListOk` (61 models, current correct),
`ChatHistory→ChatHistoryOk` (msgs, has_more=true),
`AgentAskStatus→AgentAskStatusOk` (state=unknown for a bogus id),
`ScrollbackReq→Scrollback` on a fresh shell pane. PTY echo + chat
attach/update unchanged from Phase 2. All symbols present in the release DEX.
