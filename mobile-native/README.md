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

## What it does today (v0.2.0)

- **Supabase auth** — email/password (GoTrue) with refresh; tokens persist in
  SharedPreferences. (`Auth.kt`)
- **Machine list** — `GET /rest/v1/machines_info` with the user JWT.
- **Realtime client** — hand-rolled Phoenix client over the private channel
  `realtime:machines:{machineId}` (JWT in the join payload). Ported from
  `crates/ranch/src/relay.rs`: 25 s app heartbeats, 75 s zombie-connection
  guard, JWT refresh ~2 min pre-expiry (`access_token` event + re-join),
  chunk reassembly. (`Realtime.kt`)
- **Notification engine** — port of `mobile/lib/notifyEvents.ts` +
  `mobile/lib/notifications.ts`. Fires local notifications for
  turn-end / every-message / agent-question **only when the app is in the
  background**, with the same 4 per-event toggles and replay-safe dedup.
  (`Notify.kt`)
- **Foreground service** — `MonitorService` keeps the WS alive and shows a
  persistent low-importance "monitoring …" notification. Restarts
  itself + restores the machine from Prefs on process kill (START_STICKY).
- **UI** — `MainActivity` (programmatic views): sign in, pick a machine,
  start/stop monitoring, the 4 notification toggles, permission + test
  buttons, and a live diagnostics readout (frames seen, fired/skipped/error
  counters).

## Build

Requires the Android SDK (platform 35, build-tools 35) + a JDK 17. Uses the
same `ranch` signing keystore as the RN app
(`~/.local/android-keystore/ranch.keystore`).

```sh
cd mobile-native
export JAVA_HOME=$(mise where java) ANDROID_HOME=~/.local/android-sdk
./gradlew assembleRelease
# -> app/build/outputs/apk/release/app-release.apk
# copy to ../releases/ranch-native-0.2.0.apk
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

- **v0.2.x** — notification tuning, persistent-connection reliability,
  proper app icon.
- **Phase 2** — terminal screen: render the semantic grid (Snapshot/Update,
  ANSI colors, cursor) and send encoded keys back (`Input` frames).
- **Phase 3** — sessions / agents / chat / settings.
- **Phase 4** — file editor, workflows, triggers, machines.
- Optional: **FCM** as a belt-and-suspenders true-push channel (needs a
  Firebase project + a daemon/edge-function push path).
