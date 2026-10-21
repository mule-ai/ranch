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

---

## Native app — Phase 4: full feature parity (2026-09-21)

`mobile-native/` v0.5.0 (versionCode 4) — `releases/ranch-native-0.5.0.apk`.

Five new screens, all request/response over the existing `RelaySession`
frame bus (no daemon changes):

- **Agents** (`AgentsActivity.kt`): `PiList`→`PiListOk` list (title, cwd,
  active ●/○, [ext] badge); `PiMonitor` toggle; adopt a pi conversation
  into ranch via `SessionsCreate{kind:pi, pi_session_file}`; "+ New agent
  session".
- **Files** (`EditorActivity.kt`): `DirList`/`DirListOk` browser (⌂ parent,
  📁 dirs, 📄 files), `FileRead`→monospace editor, `FileWrite` with the
  read `mtime` for conflict detection, `FilePut` uploads a phone file
  (base64) to the host, `FileChanged` auto-reloads an open file.
- **Workflows** (`WorkflowsActivity.kt`): `WorkflowList`→`WorkflowListOk`,
  per-row Details (`WorkflowGet`→`WorkflowGetOk` step list), Run
  (`WorkflowRun`), Delete (`WorkflowDelete`→Ok).
- **Triggers** (`TriggersActivity.kt`): `TriggerList`→`TriggerListOk`
  (●/○ enabled, kind · cron · workflow), Run / Delete per row, a minimal
  create-cron form via `TriggerPut`, live `TriggerFired` line.
- **Machines** (`MachinesActivity.kt`): machine list with online status
  (REST `machines_info`, 90 s freshness) + **Upgrade** button sending the
  `Upgrade` frame (hot daemon upgrade, sessions survive).

Session extras in `SessionActivity`: ✎ rename (`SessionsRename` via
dialog) and ✕ kill (`SessionsKill`) in the top bar. `Term.kt` gains
builders: `piList`, `piMonitor`, `adoptPiSession`, `dirList`, `fileRead`,
`fileWrite`, `filePut`, `workflowList/Get/Run/Delete`, `muleAgents`,
`triggerList/Run/Delete`, `sessionsKill`, `sessionsRename` (+ `PiSession`
parse). Home screen gets a Tools nav row (Agents · Files · Workflows ·
Triggers · Machines). All activities registered in the manifest.

Not yet ported (follow-ons): workflow/trigger *editing* beyond create/run/
delete (needs the full `WorkflowDraft` form), window-stack ops
(`WindowNew`/`Select`/`Next`/`Kill`/`Rename`), pane ops (`PaneSplit`/
`Resize`/`Kill`/`Swap` — blocked on the true split-pane layout), forge
resume (`ForgeList`), FCM, Google-OAuth.

---

## Native app — Phase 5: polish (2026-09-21)

`mobile-native/` v0.6.0 (versionCode 5) — `releases/ranch-native-0.6.0.apk`.

- **Google OAuth login** (`Auth.kt` + `MainActivity` + manifest): a
  "Sign in with Google" button opens Supabase's
  `/auth/v1/authorize?provider=google&redirect_to=ranch://auth-callback`
  in the system browser; the return deep link lands on `MainActivity`
  (singleTask + BROWSABLE intent-filter on `ranch://auth-callback`),
  which parses the token fragment via `Auth.applyOAuthFragment` and
  persists access/refresh like the password path. **Requires the Google
  provider enabled in the Supabase dashboard with `ranch://auth-callback`
  allow-listed** — not verifiable from here.
- **FCM: deliberately skipped** (as planned) — needs a Firebase project +
  `google-services.json` + a daemon→FCM push path. The foreground service
  owns the socket today; revisit only if it proves flaky under Doze.
- App icon: the existing PNG mipmaps are kept (a redesign is cosmetic).

## Native app — Phase 6 prep: parity matrix (2026-09-21)

RN (`mobile/`) is **deprecated but not deleted** until the native app is
confirmed working on the device. Parity after v0.6.0:

| Feature | RN | Native |
|---|---|---|
| Background notifications + 4 toggles | ✅ | ✅ (foreground service) |
| Session list / new shell session | ✅ | ✅ |
| PTY terminal (SGR grid, cursor, keys) | ✅ | ✅ + predictive echo |
| PTY scrollback | ✅ | ✅ |
| Agent chat + markdown | ✅ | ✅ (basic markdown) |
| Model picker + context readout | ✅ | ✅ |
| Agent question card (`ranch_ask`) | ✅ | ✅ |
| Agents/pi manager + adopt | ✅ | ✅ |
| File browser/editor/upload | ✅ | ✅ |
| Workflows list/run/delete | ✅ | ✅ (no edit form) |
| Triggers list/run/delete/create | ✅ | ✅ |
| Machines + upgrade | ✅ | ✅ |
| True split-pane layout | ✅ | ❌ (tabs) |
| Window stack ops | ✅ | ❌ |
| Pane ops (split/resize/kill/swap) | ✅ | ❌ |
| Forge resume | ✅ | ❌ |
| Workflow/trigger full edit forms | ✅ | ❌ |
| Google OAuth | ✅ | ✅ (needs dashboard config) |
| FCM true-push | ❌ | ❌ (foreground service instead) |

Retiring RN = delete `mobile/`, remove its docs, and note the RN-only
features above as native follow-ups. Do it only after on-device parity
sign-off.

## Native app — out-of-date banner (2026-09-23)

Port of the RN update banner (`mobile/lib/version.ts`): CI bakes
`RANCH_VERSION=main-<sha>` into BuildConfig (same string versions.json
publishes); the app fetches `ranch-dist/main/versions.json` on resume and
compares. Banner = a full-width "⬆ update available — download <latest>"
button opening the ranch.apk URL in the browser. Also ported the
daemon-version note: `HelloOk.version` is now captured into
`Monitor.daemonVersion`; when the daemon's version differs from latest,
an amber note points at `ranch upgrade`. Dev builds ("dev") skip the
banner. v0.6.9 / versionCode 13.

## Native app — compaction state survives back-swipe / process kill (2026-09-23)

The compacting banner + queued messages lived only in the Activity's
memory: a back-swipe finished the activity and the state was gone, even
though the daemon kept compacting server-side. Now the compaction state
(pane, since, queued messages) persists to Prefs keyed per session,
restores on screen create, and reconciles on the first Snapshot: the
daemon caches the last context readout on the pane, so compaction that
finished while the app was away shows up as "compacted → …" and flushes
the queue on re-attach. Restored state older than 10 min is treated as
timed out (queued messages flush with a note). Meta exited clears the
persisted state (nothing to flush into a dead session). v0.7.0 /
versionCode 14.

## Native app — copy message contents (2026-09-23)

Port of the RN copy UX: long-press a chat bubble → the raw markdown text
(not the rendered spans) goes to the clipboard, with the meta line
flipping to "copied ✓" for 1.5 s (before it: "hold to copy"). Tool rows:
tap toggles the full output (▼/▲), long-press copies the tool output —
RN parity restored. v0.7.1 / versionCode 15.

## Native app — selectable chat text (2026-09-23)

Chat bubbles and tool output now use Android's native text-selection
flow (setTextIsSelectable): long-press → selection handles → copy part
of a message, or Select All from the toolbar. Replaces the
long-press-copies-everything behavior; the meta hint says "long-press to
select". Tool titles keep tap-to-expand + long-press-copies-full-output.
v0.7.2 / versionCode 16.

## Daemon — relay wedge can no longer freeze the daemon (2026-09-23)

Live incident: the daemon deadlocked for ~2h. The relay thread hit
`WebSocket protocol error: Connection reset without closing handshake`,
reconnected to a silently-wedged TCP connection (receives appeared to
work, broadcasts never flushed), and got stranded in `ws_send`'s
unbounded `flush()` retry loop — outside the poll loop, so the 75s
zombie-connection guard could not fire. Nothing drained the
daemon→relay pipe; once its 64 KiB buffer filled, the main loop's
BLOCKING `write_all` parked in `anon_pipe_write` and every client,
local ones included, hung (`ranch ls` never returned). SIGTERM was
unusable (shutdown handler stuck in the same deadlock); recovery
required SIGKILL + restart, losing all pane children.

Two fixes, defense in depth:

- `relay.rs` `ws_send`: the flush retry is now bounded (30s). A
  connection that is not writable within 30s errors the session and
  takes the normal reconnect/backoff path. Wedged-connection detection
  is no longer dependent on TCP eventually erroring.
- `daemon.rs` `send_frame`: the relay pipe's write end is now
  `O_NONBLOCK` (set in `relay::make_pipes`). The main loop writes to it
  without blocking; frames that hit a full pipe spill into a bounded
  (192 KiB) per-client `backlog` drained by the poll loop each tick,
  oldest frames dropped on overflow. A wedged or slow relay thread can
  now at worst delay/drop remote frames — it can never again stall the
  main loop or hang local clients. Clients resync via Snapshot frames
  and the seq-gap logic on attach.

## Tool call display rework (2026-09-24)

Wire protocol: ChatMsg gains `tool_args` (pi `args` / forge
`tool_input`, compact JSON) — the command was never reaching clients,
only the result blob. pilocal captures args at tool_execution_start;
forge maps tool_input; all other ChatMsg sites None.

Native app tool rows redesigned: collapsed = `⚙ bash · <command> · 1.2s
▼` (key argument per tool: command/path/pattern); tap expands labelled
"command" + "output" mono blocks (selectable, dark boxes); output
unwraps pi's `{content:[{text}]}` envelope to plain text, red "error"
label when isError; long-press copies command+output.

Also diagnosed today's "context reporting or compaction broken" report:
neither broken — (1) default model spark/qwen3.8-sglang endpoint was
down, so fresh test sessions' turns hung and nothing accumulated;
(2) pi only compacts history older than keepRecentTokens=20000, so a
1%-context session correctly refuses ("Nothing to compact (session too
small)"). Verified live: compaction works on switched model; tool_args
round-trips. v0.7.3 / versionCode 17.

## Notification timing + composer focus fixes (2026-09-24)

Root cause of "notifications fire on the first agent message, not the
last": pi emits `turn_end` after EVERY tool round (per LLM segment), and
pilocal wrote agent "idle" on it — so the working→idle edge fired
mid-turn, after the first tool result. Fix: idle only on `agent_end`
(pi's true end-of-run), reinforce "working" on turn_start/agent_start.
Also fixes the TUI busy indicator flickering mid-turn. Phone-side:
turn-end notification now carries the last assistant message's text
(batched per turn; every_message/tool pings fold into it when turn-end
is on — verified live: one notification, at the end, body = last
message).

Composer focus steal: ScrollView.fullScroll(FOCUS_DOWN) runs a focus
search that hands focus to newly-added selectable bubbles, yanking it
from the composer mid-typing. Replaced with a pure scrollTo
(chatScrollBottom), used by renderChat + chatStickBottom.
v0.7.4 / versionCode 18.

## Notification deep link (2026-09-24)

Tapping a notification now opens the conversation that fired it.
Notify tracks pane→session from frames (all triggers carry both) and
stamps open_session/open_session_name extras on the PendingIntent
(requestCode = tag hash so stacked notifications keep their own
targets). MainActivity consumes the extras in onCreate/onNewIntent and
auto-opens the session once the monitor is up (retry on the 2s refresh
tick; cold start = login → machine pick → auto-open). Session title
resolved from Monitor.sessions. v0.7.5 / versionCode 19.

## CI fix (2026-09-25)

Commit 57bc252 swept in the uncommitted non-blocking relay-write work
(write_pipe_nb/PipeWrite/backlog). CI's rustc rejected
`Ok(n) if n > 0` + `Ok(0)` as non-exhaustive (guarded arms don't count
toward coverage) — reordered to an unguarded `Ok(n)` arm and made the
fn take &mut File. Daemon hot-upgraded.

## Deep-link pane precision + per-session notifications (2026-09-25)

Wrong-conversation report: the deep link opened the right session but
the daemon's ACTIVE pane — wrong conversation when a session has
several chat panes. Notifications now carry open_pane and
SessionActivity honors it on the first snapshot (preferredPane, then
follows the daemon again). Also: notification title is now the session
name (was the machine name) and the shade tag is the session id —
notifications from different agents stack instead of clobbering.
v0.7.6 / versionCode 20.

## Stop agent: immediate, non-destructive turn interrupt (2026-09-25)

New `Interrupt` frame (client → daemon) halts the in-flight turn of a
chat pane without killing anything: local-pi panes get pi's `abort`
RPC ("abort current operation, wait for idle") and the daemon emits a
`⏹ interrupted` system row; forge-backed panes POST to forge's new
`POST /sessions/{id}/interrupt`. The forge side writes the abort RPC
to pi's SHARED stdin (`SharedPiAgent::stdin`) — it deliberately bypasses
the per-session agent `Mutex`, which `drive_turn` holds for the whole
turn, so the interrupt never blocks on the turn it is trying to stop.
Idle interrupts are no-ops (no row); the stop is immediate and the
session, history, and pi process all survive.

Clients: native mobile `⏹ stop` button in the composer (visible while
the pane's `meta {kind:"agent", status:"working"}` says busy; hides
optimistically on tap), web `⏹ stop` button beside the composer input
(same agentBusy state), TUI `:stop` command and chat `/stop`.
Confirmation is out-of-band: the system row + the usual
`meta status:"idle"`.

Also swept the two TEMP relay diagnostics out of `send_frame`/
`write_pipe_nb` (the /tmp frame log and the EINTR eprintln).

## ranch_ask: no expiration (2026-09-25)

Asks no longer expire. Removed ASK_TTL (30 min) from the daemon's
AskRegistry: pending asks stay pending forever (the user may answer
hours later); state() is pending/answered only; prune only drops
answered asks after the 5-min grace. Extension: ranch_ask blocks
indefinitely (deadline removed); a daemon restart surfaces as
"unknown" → "no answer (ask lost)". Tool description + docs updated.
Also completed a concurrent session's in-flight image_refs work that
left the tree broken (8 ChatMsg initializers missing the field —
mechanical image_refs: None; 20 tests green).

## Images as first-class citizens (2026-09-26)

All three surfaces can now attach images to prompts, view attached
images, and see images the agent reads during chat. Selection is
both client-side and daemon-side.

- **Protocol** (`ranch-protocol`): new `FileGet`/`FileGetOk` frames
  (client fetches image bytes from the daemon) and
  `ChatMsg.image_refs` (image paths a row references). See
  PROTOCOL.md § “Images in chat (M-images)” + the `Files` section.
- **Daemon** (`crates/ranch/src/media.rs` + `daemon.rs` + `pilocal.rs`
  + `forge.rs`): persisted media registry
  (`~/.local/state/ranch/media.json`) — the set of paths a client may
  `FileGet` (uploads, image attachments, agent-read image paths). Magic-byte
  mime sniffing (png/jpeg/gif/webp/bmp/avif/heif/svg). `ChatSend`
  attachments now split: text files inline into the prompt as before;
  **image** files load into `PiImage` payloads and ride pi's RPC
  `prompt` `images` field (true vision inlining, no tool round-trip),
  or — for forge-backed panes (whose `POST /messages` is text-only) —
  become `[Attached image: <path>]` notes so the agent's Read tool
  surfaces them. Live + replay tool rows capture `tool_args` and flag
  `image_refs` when a call reads an image.
- **TUI** (`client.rs`): `🖼 name` badges on user + tool rows; `/open
  [path]` opens the system image viewer (no path = most recent image
  in the conversation).
- **Web** (`WebApp.tsx` + `frames.ts` + `styles.css`): image
  thumbnails in user bubbles and tool rows, tap → full-size lightbox
  (Esc/click to close). Composer keeps the daemon dir picker and gains
  local image upload (file picker / paste / drag-drop → `FilePut`).
- **Mobile-native** (`Term.kt` + `SessionActivity.kt` + `Monitor.kt`):
  gallery pick (`GET_CONTENT image/*`) → `FilePut` → composer chip;
  thumbnails on user + tool rows, tap → full-size view.

Tests: media module (sniff, tool-args refs, registry eviction, row
flagging), FileGet/FileGetOk round-trip. `cargo test` + clippy green;
web `tsc` + `vite build` green; native `assembleRelease` green.

## Image uploads over the relay: chunking shipped + 256 KiB cap found (2026-09-27)

Mobile image uploads >~190 KB failed with "upload timed out" while
everything else worked. Probes against the live channel pinned the
root cause: **Supabase Realtime's private-channel broadcast silently
drops frames above ~256 KiB** (256,000 B arrives <1 s; 266,240 B never
arrives, no error). The 133 KB test screenshots sat under the cap;
real photos did not. A second, older bug compounded it: the mobile
`Realtime` client never echoed server-originated `phx_heartbeat`
requests, so Supabase dropped the phone's connection every ~2-5 min
(relay log showed `Hello` spam), and frames racing the dead-socket
window were lost.

Fixes, all surfaces:

- **Heartbeat echo** (mobile `Realtime.kt`, 0.7.12): respond to
  `phx_heartbeat` on the phoenix topic with the matching ref (the
  daemon's `relay.rs` and `tools/relay-test.mjs` already did this).
  Reconnect loop gone.
- **Chunking implemented end to end** (was spec'd in PROTOCOL §5,
  half-built: TUI + mobile *receiver* existed, no one *sent* chunks):
  - `ranch-protocol`: `MAX_FRAME` 16 KiB → **192 KiB** (measured cap
    above; 192 KiB chunk + envelope stays well under 256 KiB).
    `encode_frame` now splits on **char boundaries** (the old
    byte-aligned split + `from_utf8_lossy` mangled multi-byte UTF-8
    straddling a boundary). New `MAX_CHUNKS = 512` bound: a hostile
    `n` no longer allocates (the reassemblers were `vec![None; n]`).
    `Decoder` gains a fast path (no newline rescan when the incoming
    bytes carry none — the byte-wise test was quadratic at the new
    line size).
  - Daemon `send_frame` already ran every frame through
    `encode_frame`, so **the daemon is the single chunking
    authority**; the relay now *transports lines as-is* outbound and
    **reassembles inbound** chunks with a per-session `Decoder`
    before dispatching to the main loop (a reassembled `FilePut`
    logs `relay: <- FilePut (reassembled from chunks)`).
  - Mobile `Realtime.kt` + web `relay.ts`: sender-side chunking for
    oversized frames (char-safe slices, same 192 KiB cap, `n ≤ 512`
    guard on the receive side).
- **Relay pipe backlog torn-line bug** (latent, exposed by bigger
  lines): `RELAY_BACKLOG_MAX` was 192 KiB — one 273 KB frame spilled
  its remainder into the backlog, the cap trimmed it, and the relay
  joined a *partial* line to the next frame (corrupt JSON at the 64
  KiB boundary). The backlog now tracks an in-progress front entry
  (the remainder of a line whose head is already in the pipe) that
  trim never drops; cap raised to 16 MiB (covers one 10 MiB
  upload's chunk lines; a healthy relay drains instantly, so the cap
  only bites a long-wedged relay). Regression test:
  `relay_backlog_protects_in_progress_line`.
- **`ws_send` EAGAIN on big frames** (latent): a ~197 KB WebSocket
  write on the non-blocking socket returned `WouldBlock` from
  `ws.write` itself; the `?` propagated it as a session error and
  tore the relay down (tungstenite holds the remainder in its
  internal write buffer, so the flush loop simply continues it).
- Mobile 0.7.11 (timeout + error surfacing for uploads), 0.7.13
  (chunked send path). Web deploys from the same push.

Verified with live probes: 500 KB chunked `FilePut` reassembles +
stores; a 204,982 B PNG uploaded as 2 chunks and fetched back via
`FileGet` arrives as 2 relay chunks and reassembles **sha256-identical**;
small frames still flow as single broadcasts. `cargo test` 27+11
green, clippy clean, web `tsc` green, native `assembleRelease` green.

## Durable ranch_ask surfacing (2026-09-29)

Report: ask fired in "ranch on mini", no notification, no way to see
the question → stalled session. Root cause: the ask card existed only
as a live frame — attach after the fact and it's gone; and the
foreground gate suppressed the notification whenever ANY app screen
was up.

Daemon: AskRecord now stores the question payload and pending asks are
RE-BROADCAST — on client Hello (phone reconnects) and on Attach
(opening the session). Verified live: fresh client receives the pending
ask on both paths; answer flow unchanged. Phone: Notify fires ask
notifications even in-app unless that exact conversation is on screen
(App.foregroundSession); a suppressed ask re-arms on the next
re-broadcast; Monitor tracks pendingAsks and the home session rows
show "❓ <question>". NOTE: fixes apply per-machine after `ranch
upgrade` — the stalled "ranch on mini" daemon needs upgrading.
v0.7.7 / versionCode 21.

## H6.5: specialist-dot provisioning + web_login broker (2026-10-10)

The Herd's provisioning UX + the web-auth tool. Two halves:

**Forge** (`3a943d6f`): the secret-store seam — `secrets` table
(migration 027, named-slab; values never serialize anywhere: the
`/secrets` API lists names+expiry only) + `agents.org_id` (the
wizard's memory-org label). `credentials.rs::resolve_credential_env`
resolves the agent's `credentials_scope.env_refs` at tool-execution
time (present/absent/expired, fail-closed, slot names only in
errors) and both bash paths (streaming + non-streaming) ride the
values on the container env only (`ContainerEnv.extra_env` →
`--setenv=`). The web_login broker: harness `web_login(url, slot)` →
`POST /sessions/:id/web-login` → `ranch_tool_request` card
(`{url, slot}` ONLY, "Sign me in"/"Cancel") → approval mints a
one-time handoff token (30 min, single-use) → xdg-opens
`/auth/browser-handoff` in the user's own browser → the user signs
in themselves and pastes the cookie (token = credential, no API
key) → the value lands in `secrets`; `web_login_done(slot)` probes
status. The model sees STATUS STRINGS ONLY — grep-audit test asserts
the cookie is absent from every messages row, card payload, and
response. Harness gains `web_login`/`web_login_done` in-process
tools (agent sessions; CLIENT.md).

**Ranch** (this commit): `ranch agents new` — the wizard
(`agents_new.rs`; line prompts, profile picker from
`GET /profiles`, tools allowlist, memory scope, home machine,
default wake "07:00" → "0 7 * * *"), and the mobile ＋Agent form
(`NewAgentActivity.kt` + the `AgentProvision`/`AgentProvisionOk`
frames — the daemon runs the SAME core, `provision()`: POST
/agents + best-effort mule cron wake,
`target={forge_conversation:{forge_url, api_key, agent_id}}`).
The daemon's card lane gained the `web_login` kind: an AgentAsk
"Sign me in / Cancel" round-trip (shared `agent_ask_round_trip`
helper, refactored out of the H3.5 policy lane) posted back to
`/ranch-tools/:id/result`. Docs: `agent-builder.md` §7 (flow +
invariants + v1 limits), PROTOCOL.md (the `web_login` card kind +
the AgentProvision family), AGENTS.md CLI line. Gates: forge 24
test targets + clippy/fmt + harness 63/63; ranch-2 104 tests +
clippy 0 + fmt; mobile `assembleRelease` BUILD SUCCESSFUL.

**Owed**: the plan's 24-h proactive-turn acceptance needs a real
machine with forge+mule running (no live run here); the mobile form
is raw-profile-id + all-tools in v1 (the CLI wizard is the richer
half).

## Herd H0–H6: always-on agents land end to end (2026-10-10)

Consolidated entry for the Herd initiative (PLAN-HERD.md, H0–H6.5
complete — see its status ledger + Closeout for the full per-task
record). The agent entity, the learning loop, proactivity, the
behavior plane, and the voice loop all shipped across the three
repos:

**H0 Foundations** — pi-durable vendored (ae92585d = 1.0.4); Postgres
Storage backend `durable-pg` in forge (`57d5035b`, conformance
23/23 green, kill-9 ×2 green); runtime decision recorded (pi-durable
upstream in a Node harness — kill-criteria in
`forge/docs/HERD-DURABLE-DECISION.md`); lab infra configs written
(pgvector on the forge machine, new private `mule` machine @ 8091 —
reconcile still owed); pins frozen (H0.5).

**H1 Agent entity** — forge `418c7bce` (+`8dd668ad`): migration 016
`agents` + sessions.agent_id, agent CRUD API, agent-scoped
conversations (continue/new/fork), `/active`; ranch-2 `eca27c1`:
AgentList/AgentTalk/AgentBusy frames + PROTOCOL §8.6 + `ranch agent`
CLI + dashboard Agents section; `a81a40a`: Supabase `agents` mirror
(0008) + ranchd sync; mobile-native `ecec532` registry screen;
Expo `170d606` Agents tab; mule `b2aa75e`: agents.external_ref.

**H2 Durable core** — the pi-durable Node harness (forge `147486a9`,
`7d530311` IpcClient, `5ba85553` routing flag, `002f89da` subagents +
durable timers, `89032406` compaction/reset/history/documents/hooks)
and the cutover `73c67650`: `resume.rs`/`session_replay.rs`/legacy
turn driver DELETED; the harness is the only turn path;
`dual_kill9_recover_mid_turn` green. Lab rollout owed (heartbeat
re-register).

**H3 Behavior plane (mule)** — tenancy `93aad23`;
`forge_conversation` step + per-job WS `62f409e`; wakes (cron/
webhook/event/job, `wake_log`, rate limit) `e09ddc0`; policy engine
(evaluate/memo/proposals/audit) `f4185c5` + `0293244`; forge's
before_tool policy hook `a0e8b52b` + ranch-2 `9ba9a71` (the
`policy_ask` card lane); ranch-2 `969d03c` (mule_forward webhook).

**H4 Learning** — forge `c9d4ee54` (memory tables, pgvector),
`4a2127e8` (episode capture), `dd737acf` (belief proposals +
`memory_review` card; ranch-2 `9bcb5c3` lane), mule `c217760`
(reflection workflow), forge `619918e8` + mule `e9f08ec` (memory
triggers), forge `bccb7b4c` (cross-agent signals). pgvector install
owed → memory DB tests under the skip contract.

**H5 Proactivity** — forge `883ace25` read-only research +
`research_report` card (ranch-2 `d8d5e83` lane); forge `bbdde8e1`
wake conditions (file-watch, turn-end forwarder, schedule_reminder,
`scripts/herd-wake-scenario.sh`); H5.3 Activity View: forge
`96c31aef` + mule `36d0b9e` (pause/activity) + ranch-2 poller/
`ActivityPush`/`AgentPause` + TUI `7b86899` + mobile UIs `ad8ce74`
+ `ranch pause|unpause`.

**H6 Presence & voice** — notifications `9f734ba` (both phone
clients, action buttons, per-agent mute/priority); voice input
`feb68e9` (Parakeet push-to-talk + on-device recognizer); voice
loop mule `8b402f0` + ranch-2 `f32a00f` (`ranch call`, CallActivity,
zero new frames — PROTOCOL §8.7); barge-in v2 mule `e8f4c86` +
ranch-2 `d4a07df` (echo guard, `.spike/voice-latency/`); H6.5
provisioning forge `3a943d6f` (secrets, credentials scope,
web_login broker + grep-audit) + ranch-2 `aa24619` (`ranch agents
new` wizard, mobile ＋Agent form).

**§10 sweep (this entry)**: `docs/design/herd-overview.md` (concept
architecture + sequence diagrams), SPEC.md §5.5–5.8 (agent entity,
Activity View, voice, policies), PROTOCOL §8.1 card-kinds note,
`tools/herd-demo/` (six hand-run acceptance scripts,
exit code = failed-step count), PLAN-HERD Closeout (owed
acceptances + deliberate deferrals + kill-criteria reminder).

## Expo/RN app retired — mobile-native is the only mobile client (2026-10-16)

`mobile/` (Expo / React Native, ~2,400 lines of screens + `lib/`) is
deleted. The native Kotlin client in `mobile-native/` (`dev.ranch.android`)
is now the sole Android app; CI already built only that one, so no build or
release path changed.

**Doc sweep** (RN references removed or marked historical):

- `AGENTS.md`: dropped the `mobile/` bullet; `mobile-native/` is now
  "the **only** mobile app", `applicationId` note simplified.
- `README.md`: surface diagram says "phone (native Kotlin)"; development
  tree lists `mobile-native/` instead of `mobile/`.
- `docs/SPEC.md`: parity table header → "Mobile (native Kotlin/Android)";
  file-browser bullet, Activity View surface list, and voice-input
  paragraph no longer mention Expo; repo tree lists `mobile-native/`.
- `docs/PLAN.md`: A2 notification/settings notes point at
  `SettingsActivity` + `NotifyPolicy.kt` (SharedPreferences, notification
  channels) instead of `mobile/screens/Settings.tsx` / expo-notifications;
  B3 mobile pattern references the native Machines/Workflows screens.
- `docs/design/herd-overview.md`: surface box "Android (native Kotlin)";
  the mislabeled `web (mobile/ Expo)` table row is now `web (web/)`.
- `docs/design/cloud-voice-relay.md`: fallback/parity paths are "web",
  not "web/Expo".
- `docs/research/always-on-agents-research.md` §4.3: three clients →
  TUI, web React, native Kotlin Android.
- `mobile-native/README.md` + `BUILD-APK.md`: header records the deletion
  and warns that `mobile/*.ts|tsx` citations are git-history references;
  `NotifyPolicy.kt` is now the **source of truth** for notification policy
  (it was mirrored from `mobile/lib/notify-policy.ts`); coexistence notes
  and the shared-keystore parenthetical removed.
- `mobile-native/PLAN.md`: Phase 6 marked done, with the RN-only features
  that never reached parity promoted to native follow-ups — true
  split-pane layout, window stack ops, pane ops (split/resize/kill/swap),
  forge resume, full workflow/trigger edit forms.
- `.github/workflows/release.yml`: comment updated (no functional change).
- `crates/ranch/src/pilocal.rs`: comment "both mobile apps" → "web +
  mobile".

**Not touched:** `docs/history/` entries (including the Phase-6 parity
matrix of 2026-09-21) and the research doc's footnote about RN
notification limits — append-only record.

## H6.5b: the herd agent editor (2026-10-17)

The phone could create a dot (＋Agent) and delete one (✕) but never
change one; the CLI had no edit path either. Shipped the editor as a
frame family plus two clients on the shared core:

- **Protocol** (`ranch-protocol`): `AgentUpdate`/`AgentUpdateOk` and
  `WakeList`/`WakePut`/`WakeDelete` (+ `WakeInfo`); `AgentInfo` grows
  the editor's prefill tail (`primary_profile`, `memory_scope`,
  `org_id`, `visibility`, `tools_allowlist`, `extra_instructions`,
  `paused`) — additive optional fields, old clients drop them.
  **Absent = "leave the column alone"** (forge PATCH treats null and a
  missing key the same): no nullable column can be cleared from a
  client, so the editors' blanks mean "keep".
- **Shared core** (`crates/ranch/src/agents_new.rs`):
  `UpdateRequest`/`update_agent_body`/`validate_update`/
  `update_agent`, the single `agent_info_from_value` decoder used by
  `GET /agents` (forge worker), the PATCH reply and the CLI;
  `get_agent`/`resolve_agent_id` (name → id); wake CRUD
  (`list_wakes`/`put_wake`/`delete_wake` + `mule_req`) with the
  ownership match on `target.agent_id` ∪
  `target.forge_conversation.agent_id` ∪ `spec.agent_id`, and the rule
  **blank cron = "leave the schedule alone" on update** (pause/resume;
  non-cron kinds have no `spec.expr`) while create requires one.
- **Daemon**: `Frame::AgentUpdate` arm + `handle_wake` (mule proxy),
  new `reply_to` helper; validation runs before any HTTP.
- **CLI**: `ranch agents edit <name|id>` (current values as
  `[brackets]`, `*` = allow all tools); `agents_new::run(&[String])`.
- **Mobile**: `EditAgentActivity` (✎ per registry row) — the agent
  fields with a diff-only Save, plus the **wakes** section (edit
  schedule/prompt/name, pause/resume, delete, ＋ add wake, saved
  per-row). New `AgentForm.kt` extracts the profile picker +
  inline-`ProfilePut` form + tools checkbox set from `NewAgentActivity`
  so builder and editor cannot drift; `Term.kt` gains the four builders
  and the `AgentInfo`/`WakeInfo` parsers.
- **Docs**: PROTOCOL §8.6 (family + semantics), design/agent-builder.md
  §7.5, SPEC parity row (web still has no ＋Agent/editor form),
  AGENTS.md CLI line, mobile-native README.

Gates: `cargo test -p ranch -q` 85, `cargo test -p ranch-protocol -q`
14, clippy 0 (pre-existing `voicecall.rs` collapsible_if only), `cargo
fmt --all` clean (normalized three pre-existing mis-indented
`background: None` lines in `mule.rs`/`pilocal.rs`), `./gradlew
assembleRelease` BUILD SUCCESSFUL (APK 1.39 MB). Frames + CLI verified
against a mock forge/mule on an isolated `HOME` + throwaway daemon
(patch body, wake filter/create/update/delete, error paths). On-device
smoke of the ✎ screen is still owed.

---

## 2026-10-19 — Subagent sandbox inheritance + anchored-column sandbox-escape fix (forge `34ef5589`…`cf1adebc`, lab `1333fcd2`+)

**The escape (live ~2.5 h, fixed same day).** Migration 031's
workspace-binding feature persisted the resolved `working_dir` onto
EVERY new conversation row. The streaming tool path inferred
"anchored" (host-side, unsandboxed bash) from
`working_dir IS NOT NULL` — so from the 003286d6 deploy (~16:52 UTC),
every fresh agent conversation ran harness bash as root ON THE FORGE
VM HOST. Confirmed with a probe (`/opt/forge-hotfix` visible, uid 0).
Probe conversation + artifacts deleted.

**Fix — explicit columns (migration 032):**
- `sessions.anchored BOOLEAN` — set TRUE only by the real cwd-anchor
  paths (`create_session`/`create_conversation` with `working_dir`);
  the sandbox-vs-host decision in `stream_tool_execution` reads the
  column, never the inference.
- `sessions.sandbox_inherit BOOLEAN` — subagent children execute in
  the parent's sandbox EXACTLY: `sandbox_key_for_session()` walks the
  inherit chain (cycle-guarded, ≤10 hops) and every container lookup
  — streaming bash (`execute_bash_streaming(sandbox_key)`), executor
  bash (`ToolExecutor::with_sandbox_key`), package reconciliation,
  `get_or_create_container` — keys on the nearest non-inheriting
  ancestor. Audit rows stay attributed to the CHILD. Children also
  inherit `agent_id` (credential env_refs resolve) and the parent's
  `working_dir` (checkouts visible both ways; parent/child execs
  serialize on the nspawn tree flock instead of EBUSY-colliding).
- 032's backfill over-flagged workspace rows as anchored → corrected
  in 033 (a real anchor differs from BOTH the convention path AND the
  agent's declared `workspace_dir`). Mid-session lesson: sqlx
  checksums APPLIED migrations — editing 032 boot-looped forge-api
  ("migration 32 … modified"); 032 restored byte-exact from the
  applied commit and the refinement shipped as 033.
- `dispatch_message` self-heals workspace bindings: agent rows whose
  conversation lost/never had `working_dir = agents.workspace_dir`
  re-bind at message time (fixes the sticky steward conversation's
  `wd=''` relic without a re-mint).
- `agents.create_conversation` persists `working_dir` only for real
  workspace bindings / host fallbacks — the per-session convention
  path stays column-NULL again.

**E2E on the live hot-patch (rev `cf1adebc`):** parent bash →
`SANDBOXED`, container hostname, uid 0-in-container; synthetic child
(`sandbox_inherit`, parent's id) → SAME hostname + cwd, writes visible
parent⇄child both directions, audit row keyed to the child with the
parent's `agent_id`; 6 parallel parent+child execs → no EBUSY;
pollution counts zero; anchor-test rows unaffected. New test binary
`subagent_sandbox_tests` (3: plain-not-anchored, anchor-sets-column,
resolver chain + cycle) + lib 187 green.

**Ops notes:** `fbuild4.sh` runs INSIDE the forge VM from the home
mount (`container machine run --root -n forge --
/run/current-system/sw/bin/bash /home/jbutler/fbuild4.sh`);
`forge-ct.tar.gz` = re-tar of `alcbld/repo/ct/forge` (lab's ct flake +
vendored `forge-src-<rev>.tar.gz`; `git archive --prefix=jbutlerdev-forge-<rev>/`
is byte-stable); alcbuild2 egress can die — `container machine stop`
+ auto-start on next run clears it.

### 2026-10-19 (late) — herd_roster prompt section + sandbox git fail-fast

EA "couldn't see" Shepherd: permissions fine (same owner, no
allowlist, GET /agents healthy) — the sticky 126k-entry conversation
was quoting its last cached `list_agents` toolResult, which predated
Shepherd's row; control-plane wobble during the day's deploys pushed
the model onto the stale list. Structural fix (forge `1938bcd1`,
harness): per-turn `herd_roster` section (memory_signals pattern) —
re-fetches GET /agents EVERY render (name/(you)/home8/PAUSED/role +
"trust this over older list_agents output"); fetch failure ⇒ section
omitted, never fails the turn. `list_agents` rows + `read_agent`
header now carry the agent description (role). vitest 82/82 (18
files, +3 herd-roster incl. the per-render re-fetch assertion).

Deployment: forge-harness hot-patched like forge-api — new
`fbuild5.sh` builds `.#packages.aarch64-linux.forge-harness`, stages
to `/opt/forge/harness-patched` (dist mirrored to `<root>/dist/main.js`
for the patched-unit ExecStart; @forge/durable-pg symlink repointed
absolutely), restarts the unit. Old tree kept at `harness-old`.

Bonus catch from the roster E2E turn: the steward's own
git-push+gh-pr-create (it is now pushing F-00x fixes — PR
`fix/graceful-drain-timeout-and-staging-validation`) hung on git's
no-TTY credential prompt for 17+ min holding the sandbox tree flock —
that IS the live "Directory tree busy" (F-005) shape. forge `cb7972db`:
nspawn now sets `GIT_TERMINAL_PROMPT=0` ⇒ git errors immediately
instead of hogging the tree until the exec timeout. Verified on the
hot-patch binary (env present in a live exec, sandbox still sound).

---

## 2026-10-21 — herd audit + the vendored-tarball incident (H7.x)

Full herd audit ("are they waking and acting?"). Found and fixed:

1. **Vendored forge-src tarballs were mislabeled** (lab `ct/forge`).
   All nine `forge-src-<rev>.tar.gz` were `tar czf` of a STALE
   checkout (worktree mtimes; tree predated the agents API), not
   `git archive <rev>`. Consequence: the Oct 19 22:45 hotfix deploys
   (fbuild4/fbuild5) shipped an ancient forge-api + harness — every
   herd route (`/agents`, `/bugs`, roster, agent CRUD tools) fell
   through to the SPA fallback for ~21h while wake dispatch kept
   firing `ok=t`. Regenerated ALL tarballs with
   `git archive --prefix=jbutlerdev-forge-<rev>/ <rev>` + canary
   checks (agents routes present, migration 033, GIT_TERMINAL_PROMPT).
   lab `7f806f7b`. Redeployed forge-api (store `a98zjssm…`) + harness
   (`fihgh08f…`) from correct source. NOTE: forge's HTTP surface is
   FLAT (`/agents`, `/health`) — `/api/v1/*` is MULE's prefix; an
   earlier mis-diagnosis chased `/api/v1/agents` (which legitimately
   404s→SPA).
2. **Steward auto-compaction dead-lock**: the pi.compaction
   summarizer request itself exceeded the 262k model context on the
   huge sticky conversation → failed every turn, silently. Triaged by
   re-minting the sticky session (`83d871e8`, fresh durable
   conversation); harness fix (bounded summarizer input + truncation
   fallback + ⚠ surfacing) via subagent.
3. **Wake prompt backlog**: identical `[mule wake <id>]` prompts
   queue while an agent is mid-turn (14 unanswered submissions; the
   agent wasted turns commenting on its own backlog). Coalescing fix
   (same-wake pending ⇒ drop, cap 8 pending/session) via subagent.
4. **F-002 confirmed live**: wake-mule-dependabot `rate_limited`
   storm (2+ fires/s on Oct 19 21:39-21:46) — failed fires didn't
   advance next_due_at. Scheduler fix (advance unconditionally +
   backoff floor) via subagent (mule repo).
5. **credentials NULL-owner hard-fail** (found BY the steward, F-010):
   `session_owner_and_agent` decoded user_id non-null; a session
   minted without user_id made EVERY tool call 500. forge `fa5d423f`:
   decode as Option; NULL owner/agent ⇒ "nothing declared".
6. **Steward prompt hardened** with a control-plane self-check it now
   runs FIRST (agents-list JSON canary, health, compaction failures,
   unanswered-submission cap, wake_log hot-loop detection, wake→reply
   liveness) — it slept through the 21h outage this time.
7. Hygiene: `reflection-episodes-ready` wake disabled (no emitter —
   dead config); 11 probe sessions deleted; **CEO agent paused**
   (no wakes, no instructions, dormant since Oct 19 — needs purpose
   or deletion).

Verified healthy: all 8 wakes fire on cadence (wake_log ok=t),
dependabots produce real PR/CI sweeps, EA drives delegates, episodes
capture, steward self-reports via AUDIT.md + notify.

### Same-day follow-through (subagent fixes landed)

- **mule F-002** → PR #141 merged (16-commit backlog + scheduler fix on
  main); arm64 binary hot-patched to the mule VM (`/opt/mule-hotfix`,
  unit `active`, 7 wakes armed). CI-hygiene follow-up PR #143 (lint
  cleanups + the reflection-wasm `-o` cwd bug that fresh trees exposed:
  `go build` ran with `cmd.Dir`=repo root so the test-cwd-relative `-o`
  wrote wasms OUTSIDE the checkout — prebuilt local `dist/` had masked
  it). ATE needs an approving review (branch protection blocks admin
  merge).
- **forge compaction hardening** `7f834d14` (subagent): bounded
  summarizer input (trim-oldest to token budget), truncation fallback
  when the summarizer still fails (conversation always becomes
  compactable), ⚠ chat-row surfacing on the fallback path,
  compaction-guard vitest (19/19 harness files green).
- **forge wake-prompt coalescing** `e8c144f2` (subagent, cherry-picked
  clean over the compaction commit): `wake.rs` pure gate
  (`[mule wake <id>]` parse, duplicate-pending drop → `{"coalesced":
  true}`, pending cap 8 → 429, settled rows never suppress re-fires,
  fail-open on query error) + 4 live integration tests; lib 192/192,
  clippy baseline-clean.
- **Deploy chain**: lab flake `e8c144f2` (vendored tarball regenerated
  via git archive + canary), forge-api + harness redeployed
  (`63wvh8z9…` / harness-patched), live verify: `/agents` 200 JSON,
  compaction guards present in dist, steward 21:05 wake ran the new
  control-plane self-check (health canaries, log-gap backfill,
  err/warn counts) — herd autonomy restored end to end.
- Ops notes: two concurrent subagents on one repo (forge) crossed
  commits — subagent tasks in one repo must own DISJOINT paths or run
  sequentially; a killed duplicate runner kept writing until SIGKILL'd
  and its scratch test got swept into a `git add -A` (cleaned in #143);
  `pgrep/pkill -f <pattern>` self-matches the invoking shell — match on
  process tables instead.
