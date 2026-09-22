# mobile-native (Ranch Native)

Native Android (Kotlin) client that runs alongside the React Native app in
`../mobile/`. Goal: a lean app whose **primary** job is to hold a Supabase
Realtime connection in the background (via a foreground service) and fire
local notifications for agent events on every parallel session. The full
terminal/chat UI is a later phase; the RN app remains the UI reference.

## Why this exists

The RN client could not reliably deliver background notifications: when the
app is backgrounded Android suspends its WebSocket, and when it is in the
foreground the app deliberately suppresses notifications (the on-screen UI
already shows everything). A native foreground service sidesteps that: the
process stays alive (exempt from Doze), the WS stays open, and notifications
fire while the phone is in a drawer.

## What it does today (v0.4.0)

- **Supabase auth** — email/password (GoTrue) with refresh; tokens persist in
  SharedPreferences. (`Auth.kt`)
- **Machine list** — `GET /rest/v1/machines_info` with the user JWT.
- **Realtime client** — hand-rolled Phoenix client over the private channel
  `realtime:machines:{machineId}` (JWT in the join payload). Ported from
  `crates/ranch/src/relay.rs`: 25 s app heartbeats, 75 s zombie-connection
  guard, JWT refresh ~2 min pre-expiry (`access_token` event + re-join),
  chunk reassembly, **and outbound frames** (`Hello`/`Attach`/`Resize`/
  `Input`/`ChatSend`/`Detach`) sent as Realtime `broadcast` messages and
  queued until the channel is joined. (`Realtime.kt`)
- **Notification engine** — port of `mobile/lib/notifyEvents.ts` +
  `mobile/lib/notifications.ts`. Fires local notifications for
  turn-end / every-message / agent-question **only when the app is in the
  background**, with the same 4 per-event toggles and replay-safe dedup.
  (`Notify.kt`)
- **Foreground service** — `MonitorService` keeps the WS alive and shows a
  persistent low-importance "monitoring …" notification. Restarts
  itself + restores the machine from Prefs on process kill (START_STICKY).
- **Main screen** — `MainActivity` (programmatic views): sign in, pick a
  machine, start/stop monitoring, the 4 notification toggles, permission +
  test buttons, a **live session list** (from `HelloOk` + `SessionsAck` +
  `Meta exited`), a "+ New shell session" button, and a live diagnostics
  readout.
- **Session / terminal screen** — `SessionActivity`: attaches to one
  session and renders its active pane.
  - **PTY panes** → `TerminalView` (custom `View`) paints the SGR-tagged row
    strings into a monospace cell grid with per-cell fg/bg, bold/underline,
    and a blinking cursor block. (`TerminalView.kt`, `Sgr.kt`)
  - **Agent panes** (pi/forge, `kind=forge-chat`) → a chat list (role labels,
    collapsed tool calls, model line) with a send box that posts `ChatSend`.
  - **Input capture** — PTY: a sentinel-space `EditText` (backspace→DEL,
    newlines→CR) + a row of special keys (arrows, Enter, Esc, Tab, Ctrl-
    C/D/L) that emit the same byte sequences as the RN client; chat: a plain
    message box.
  - **Geometry** — the view computes cols/rows from its pixel size and sends
    `Resize` (canonical size follows this client, tmux-style). Multi-pane
    sessions get a tab row; tapping a tab sends `PaneSelect`.
  - **Resync** — per-pane `seq` gap → re-`Attach` so the daemon re-snapshots.
  - **Predictive echo** — typed chars paint dimmed at the cursor and clear
    when the authoritative `Update` for that row lands.
  - **PTY scrollback** — `hist` key sends `ScrollbackReq` → `Scrollback`
    rendered in a popup.
  - **Markdown** — chat messages render `**bold**`/`*italic*`/inline code/
    headers/lists via `SpannableStringBuilder`.
  - **Chat scrollback paging** — scrolling to the top fires `ChatHistory` →
    `ChatHistoryOk`; older rows prepend.
  - **Model picker** — a chip shows the pane's model + context; tapping sends
    `ModelList` → `ModelListOk` and a popup lets you `ModelSet`.
  - **Agent question card** — `AgentAskRequest` renders an interactive card
    (choices + suggested + free-text + multi) pinned above the input; Send
    posts `AgentAskAnswer`. The phone UI for the `ranch_ask` agent tool.

## Build

Requires the Android SDK (platform 35, build-tools 35) + a JDK 17. Uses the
same `ranch` signing keystore as the RN app
(`~/.local/android-keystore/ranch.keystore`).

```sh
cd mobile-native
export JAVA_HOME=$(mise where java) ANDROID_HOME=~/.local/android-sdk
./gradlew assembleRelease
# -> app/build/outputs/apk/release/app-release.apk
# copy to ../releases/ranch-native-0.4.0.apk
```

- AGP 8.12.0, Gradle 9.4.1, Kotlin 2.2.10.
- `applicationId` is `dev.ranch.android` (distinct from the RN
  `dev.ranch.app`) so both apps coexist on one device during migration.

## Wiring it to the daemon

No daemon changes needed: the daemon already broadcasts agent `Meta`
(working/idle), `Chat`, and `AgentAskRequest` frames to **all** connected
clients (see commit `bf045b8`). The native client simply joins the same
private channel the RN app uses, with the same user JWT.

## Roadmap

- **v0.4.x** — CJK/wide-char metrics, multi-pane split layout instead of tabs,
  on-device IME tuning, Google-OAuth login, proper app icon.
- **Phase 4** — agents/pi manager, file editor, workflows, triggers, machines.
- Optional: **FCM** as a belt-and-suspenders true-push channel (needs a
  Firebase project + a daemon/edge-function push path).

## Verification (protocol round-trip)

The exact frame flow the app uses was validated against a live daemon:
local-socket `Hello→Attach→Snapshot→Resize→Input(echo)→Update` (PTY path)
and Realtime `join→Hello→HelloOk→Attach→chunked Snapshot` (chat path).
Phase-3 frames also validated live: `ModelList→ModelListOk`,
`ChatHistory→ChatHistoryOk`, `AgentAskStatus→AgentAskStatusOk`,
`ScrollbackReq→Scrollback`.
