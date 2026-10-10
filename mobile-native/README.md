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
  turn-end / agent-error / every-message / agent-question **only when the
  app is in the background**, with the same per-event toggles and
  replay-safe dedup. Agent-error rows (assistant text starting with `⚠`,
  emitted by the daemon when a pi turn fails) are default-on and suppress
  that pane's turn-end notification. (`Notify.kt`)
- **Foreground service** — `MonitorService` keeps the WS alive and shows a
  persistent low-importance "monitoring …" notification. Restarts
  itself + restores the machine from Prefs on process kill (START_STICKY).
- **Main screen** — `MainActivity` (programmatic views): sign in, pick a
  machine, start/stop monitoring, the notification toggles, permission +
  test buttons, a **live session list** (from `HelloOk` + `SessionsAck` +
  `Meta exited`), a "+ New shell session" button, and a live diagnostics
  readout.
- **Agents screen** — `AgentsActivity`: the forge agent registry (Herd
  H1.4). Sends `AgentList` over the same Realtime channel; each
  `AgentListOk` row shows an avatar placeholder, the agent name, a live
  busy dot (`AgentBusy`/`AgentBusyOk`), home machine + last active. Tap
  a row → `AgentTalk{mode:"continue"}` → on `AgentTalkOk` the new
  session opens in [SessionActivity] (same extras as the home screen
  session list). `Error` replies (unknown agent, agent lives on another
  machine, forge down) flash the status line — never crash. The
  pre-H1.4 **local pi manager** (PiList/PiMonitor/adopt) is kept as a
  second section under a "Local pi sessions" divider.
- **Activity screen** — `HerdActivity`: the H5.3 Activity View (the
  herd's live timeline, mirroring the TUI's Activity section). Data is
  push-only: the daemon's activity poller broadcasts `ActivityPush`
  {items} on every item-set change and this screen replaces its
  in-memory timeline (no polling, no persistence in v1). Items group
  by agent (machine-wide items — the local ask/spawn/trigger/mule_job
  kinds, `agent: null` — group under "LOCAL"), groups ordered by their
  most-recent item, each row "glyph text · HH:MM" with the TUI's
  H5.3 glyph map (🧠 belief, ✅ ask, 🔬 research, ⏰ wake, ▸ forge
  task, ⇄ mule job, ⚡ trigger, ✚ spawn). Header **Pause herd** /
  **Resume herd** button sends `AgentPause { agent: null, paused }`
  (the global kill switch: forge pause + mule wake-pause); long-press
  on an item offers "Pause agent <name>" / "Resume agent <name>"
  (machine-local items flash instead). Both toggle off the last
  CONFIRMED target (`AgentPauseOk` with all legs ok sets/clears the
  hint; a failed leg leaves it unknown) and toast the per-agent
  results. Tapping an item deep-links to that agent — the H1.4
  `AgentTalk{mode:"continue"}` path (name → id from a cached
  `AgentList`, so the screen sends one on open); machine-local items
  flash "open it from the sessions list" (v1: no cross-pane focus).
  Opened from the Settings & tools screen's Tools row (next to
  Agents).
- **Per-agent notifications** — `NotifyAgentsActivity` (H6.1): the
  forge registry with a **mute** switch + **high/normal/quiet**
  priority per agent, persisted as the `agent_notify` JSON pref
  `{name: {muted, priority}}`. Mute is notification-only — the
  agent's items still appear in the Activity View. quiet/high pick
  the low/high-importance notification channels. The shared
  notification POLICY (kinds, defaults, card-lane choice
  fingerprints, action labels) lives in `NotifyPolicy.kt`, mirrored
  from the Expo app's `mobile/lib/notify-policy.ts` (source of
  truth — keep in sync). Notification buttons (Keep/Forget,
  Use/Discard, Allow/Deny — the ask's first two choices) route
  `AgentAskAnswer` through the foreground service's Realtime socket
  (`MonitorService`); task-done notifications carry **Open** / **Fork**
  (`AgentTalk{mode:"continue"|"fork"}` — the plan's **Stop** button is
  deferred: the `TaskAbort` frame does not exist yet, matching
  H5.3's v1). Agent identity on notifications comes from
  `AgentTalk`/`AgentTalkOk` pane-binding + the `AgentListOk`
  registry (Meta/Chat/AgentAskRequest frames carry no agent id);
  unresolved panes notify under the machine name. Avatars: registry
  `avatar_url` downloaded to `cacheDir/avatars/` (`Avatars.kt`,
  plain OkHttp — no new dep) and shown via `setLargeIcon` +
  `BigPictureStyle` (custom bitmaps are not allowed as small icons
  on modern Android). Opened from the Settings & tools screen's
  Tools row (**Per-agent**).
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
  - **Voice input (H6.2)** — a 🎤 button next to send in the chat input row,
    with two modes (Settings & tools → *Voice input*, Prefs `voice_input`):
    - **forge-stt (default)** — push-to-talk: ACTION_DOWN records a
      16 kHz mono m4a/AAC memo (`MediaRecorder`, `DictationRecorder` in
      `Voice.kt`), ACTION_UP stops it and the bytes go straight to your
      forge's OpenAI-compatible STT proxy, `POST {forge}/v1/audio/transcriptions`
      (multipart `file` + `model=parakeet` + `response_format=json`,
      `X-API-Key` auth; OkHttp, 30 s timeout) → Parakeet → the transcript
      appends to the composer. This is the path the H6.3 voice loop uses.
      It needs the **forge base URL + API key in Settings** — the phone talks
      to forge *directly* on this path (there is no forge channel through
      the daemon), so it's the config asymmetry: recognizer needs nothing,
      forge-stt needs those two fields.
    - **recognizer** — Android's system `SpeechRecognizer`
      (`RecognizerIntent`, `EXTRA_PREFER_OFFLINE`) → transcript into the
      composer. No new deps, no config, and it's the **airplane-mode path**
      (no network at all).
    - v1 behavior: the transcript lands **in the input box — you still hit
      send** (no auto-send). While recording the button turns red ⏹ and the
      status line says so; a level meter is deliberately out of scope.
    - **Permissions**: `RECORD_AUDIO` is requested at first forge-stt use
      (runtime flow); if denied, a toast + status flash and the tap falls
      back to the recognizer once. forge-stt misconfig/5xx (e.g. forge's
      503 "speech-to-text is disabled" / 502 "Parakeet STT backend
      unreachable") flash the status line — never a crash (M8.4 rule).
  - **Voice call (H6.3)** — a 📞 button on every row of the Agents screen
    (`CallActivity`): a push-to-talk *call* with the agent, orchestrated
    by a mule `voice_call` workflow. The phone dials **mule directly**
    (same transport decision as `ranch call` — no ranch frames, the
    daemon is out of the media path; see PROTOCOL.md §8.7):
    - **Setup**: Settings & tools → *Voice input* gains **Mule base URL +
      API key** (the same pair as `daemon.toml`'s `mule_url`/`mule_api_key`).
      With those, 📞 looks up the agent's `voice_call` workflow
      (`GET /api/v1/workflows` + its steps, matching `config.agent_id`),
      starts the job (`POST /api/v1/jobs`), and opens two WebSockets.
    - **Audio in**: `AudioRecord` 16 kHz mono s16le, 30 ms binary frames
      → `ws {mule}/voice/{job}/in`. If the mic permission is denied, a
      typed-text row appears instead (one line = `{"text":…}` utterance
      frame, no STT hop; "bye" hangs up).
    - **Audio out**: `ws {mule}/voice/{job}/out` binary TTS PCM →
      `AudioTrack` (sample rate from the step's `tts_sample_rate`,
      default 24 kHz — the out socket carries no rate metadata); the
      `{"assistant":…}` text frames append to the on-screen transcript
      (so a call still works with no speaker); `{"done":true}` = ended.
    - **Barge-in v1** needs nothing here: mule's in-socket VAD triggers
      on your voice while the TTS plays; the screen just keeps capturing.
    - **Hang-up**: the Hang up / back button sends `{"hangup":true}` and
      closes both sockets; mule also treats a closed in socket as a
      hang-up. A mule-side crash shows "call dropped" in the transcript
      (the conversation itself is intact — turns commit as they land).
    - No new permission: `RECORD_AUDIO` was already declared by H6.2.

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
