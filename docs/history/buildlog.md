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
