# mobile-native — migration plan (RN → native Kotlin)

Goal: a native Android client that is a full replacement for `../mobile/`
(React Native/Expo), with reliable background notifications as the reason
it exists at all. This document is the source of truth for scope and
ordering. Update it as phases ship.

## Why native (recap)

The RN app cannot reliably notify: backgrounded → Android suspends its
WebSocket; foreground → notifications are intentionally suppressed (the
on-screen UI already shows everything). A native **foreground service**
holds the Supabase Realtime socket (Doze-exempt), so agent-event
notifications fire while the phone sits in a drawer. Everything else is a
byproduct of making the native client a real client.

## Architecture (invariant across phases)

- **One wire protocol.** Every transport (local unix socket, Supabase
  Realtime relay, this app) speaks the same JSON frames defined in
  `crates/ranch-protocol`. No app-specific protocol.
- **The daemon is the single source of truth** for terminal/agent state.
  The phone never emulates; it renders `Snapshot`/`Update`/`Chat` and sends
  `Input`/`ChatSend`/control frames.
- **The foreground service owns the socket.** UI activities register as
  frame sinks on `Monitor` (`RelaySession`); they never own the connection.
  Killing an activity never drops the socket. Notifications keep firing
  regardless of which screen (if any) is open.
- **Outbound frames** travel as Realtime `broadcast {event:"frame",
  payload:<frame>}`; frames sent before the channel joins are queued and
  flushed on join (so `Hello→Attach→Resize→Input` stay ordered).
- **Frame shapes** are validated against a live daemon with the two scripts
  in the build log (local-socket PTY echo; Realtime join→attach→chunked
  snapshot). Do not add a frame to the app that the daemon does not emit/
  accept.

## What's ported, and where

| Capability | RN source (lines) | Native | Status |
|---|---|---|---|
| Supabase GoTrue auth + refresh | `lib/supabase.ts`,`lib/config.ts` | `Auth.kt`,`Supabase.kt` | ✅ P1 |
| Machine list | `lib/config.ts`+PostgREST | `Auth.machines()` | ✅ P1 |
| Realtime Phoenix client (join/hb/zombie/chunk) | `lib/relay.ts` (130) | `Realtime.kt` | ✅ P1+P2 |
| Notification engine (4 toggles, dedup, bg-only) | `lib/notifyEvents.ts`(136)+`lib/notifications.ts`(238) | `Notify.kt` | ✅ P1 |
| Foreground service (Doze-exempt, START_STICKY) | n/a (RN can't) | `MonitorService.kt`,`Monitor.kt` | ✅ P1 |
| SGR row parser (colors/bold/underline) | `lib/sgr.ts` (127) | `Sgr.kt` | ✅ P2 |
| Frame builders + b64 + types | `lib/frames.ts` (309) | `Term.kt` | ✅ P2 (subset) |
| Terminal grid renderer + cursor | `screens/Terminal.tsx` (1609, render part) | `TerminalView.kt` | ✅ P2 (single pane) |
| PTY key capture + special-key row | `screens/Terminal.tsx` (input part) | `SessionActivity.buildPtyInput` | ✅ P2 |
| Agent chat list + `ChatSend` | `screens/Terminal.tsx` (chat part) | `SessionActivity` chat path | ✅ P2 (plain text) |
| Session list + new session | `App.tsx` session list | `MainActivity` | ✅ P2 |
| Attach/Resize/PaneSelect/seq-resync | `screens/Terminal.tsx` | `SessionActivity` | ✅ P2 |

Still to port: the remaining ~2,400 lines of RN screen code (Agents,
Editor, Workflows, Triggers, Machines, Settings) + the RN `Terminal.tsx`
polish (predictive echo, markdown, scrollback, split layout).

## Phase 3 — make the terminal + chat feel right (the long pole)

> **Status (0.4.0):** 3b done — markdown chat, chat scrollback paging,
> model picker + context readout, agent-question card (all frames validated
> against the live daemon). 3a done — predictive echo + PTY scrollback
> (`hist` key). Deferred: true split-pane layout (still active-pane tabs),
> CJK/wide-char metrics (ASCII-only for now), on-device IME tuning.

This is the phase that turns "it works" into "I actually use the phone."
Ordered by user value.

### 3a. Terminal feel (PTY panes)
- **Predictive echo.** RN shows unconfirmed typed chars dimmed at the
  cursor (`pred` state in `Terminal.tsx`). Port it: on `Input`, remember the
  chars + cursor; clear when the authoritative `Update` for that row lands.
  Frames: none (pure client). This is what makes typing over the relay feel
  instant.
- **True split-pane layout.** Replace the pane *tabs* with a real split
  layout: walk the `Snapshot.layout` `Layout` tree (Leaf/Split{dir,pct,a,b})
  into nested rects (the RN `layoutRects` fn), render one `TerminalView`
  per leaf, route `Input` to the focused leaf. Frame: none (already have
  `Layout`).
- **Scrollback.** On scroll-up, page `ScrollbackReq{offset,limit}` →
  `Scrollback{lines}`; render above the live screen. Frames:
  `ScrollbackReq`/`Scrollback`.
- **Wide/CJK metrics.** `TerminalView` currently assumes 1 char = 1 cell.
  Use `Paint.breakText`/`Character.isHighSurrogate` + East-Asian Width so
  2-cell glyphs align. (Defer if the user's workflows are ASCII-only; low
  risk otherwise since content is server-emitted.)
- **Cursor/IME tuning on real hardware.** The sentinel-`EditText` capture
  is the part most likely to need per-keyboard tweaks (some IMEs buffer).
  Keep the 1px-transparent trick; verify on Gboard + at least one other.

### 3b. Agent chat (forge-chat panes)
- **Markdown rendering.** Port `lib/highlight.ts` (255 lines) — headers,
  bold/italic, inline code, fenced blocks, lists — to a styled
  `SpannableStringBuilder` (no WebView). This is the biggest single
  remaining chunk of "looks like the RN app."
- **Chat scrollback paging.** `ChatHistory{before,limit}` → `ChatHistoryOk`
  when the user scrolls up past the loaded tail (RN `chatLoadingOlder`
  logic + `chatCache.ts`). Frames: `ChatHistory`/`ChatHistoryOk`.
- **Model picker + context readout.** `ModelList{pane,req_id}` →
  `ModelListOk`; tap → `ModelSet`. Show the `meta kind="context"` readout
  (already received; not yet displayed). Frames: `ModelList`,
  `ModelListOk`, `ModelSet`, `Meta context`.
- **Agent question card.** Render `AgentAskRequest` as an interactive card
  (choices + suggested + free-text + select-1/many) and post
  `AgentAskAnswer`. This is the `ranch_ask` tool's UI. Frames:
  `AgentAskRequest`, `AgentAskAnswer`. (Notifications for these already
  fire via P1.)

## Phase 4 — full feature parity

Bring the remaining RN screens over, one by one. Each is a self-contained
screen + a handful of request/response frames; all reuse the existing
`RelaySession` frame bus.

- **Agents / pi manager** (`screens/Agents.tsx`, 451): `PiList`/`PiListOk`,
  `PiMonitor`/`PiMonitorOk` (adopt/stop external pi sessions), resume via
  `SessionsCreate{pi_session_file}`.
- **File editor** (`screens/Editor.tsx`, 702): `DirList`/`DirListOk`
  (browse), `FileRead`/`FileReadOk`, `FileWrite`/`FileWriteOk`,
  `FilePut`/`FilePutOk` (upload from phone), `FileChanged` (live re-load).
- **Workflows / mule** (`screens/Workflows.tsx`, 273): `WorkflowList`/
  `Get`/`Put`/`Delete`/`Run` + `MuleAgents`/`MuleAgentsOk` (step agents).
- **Triggers** (`screens/Triggers.tsx`, 305): `TriggerList`/`Put`/`Delete`/
  `Run`/`Fired`.
- **Machines** (`screens/Machines.tsx`, 103): list/online-status (already
  have the list), `Upgrade` frame (hot-upgrade the daemon from the phone —
  mirror the RN "upgrade" button).
- **Sessions screen extras** (`App.tsx`): rename (`SessionsRename`), kill
  (`SessionsKill`, have it), window stack (`WindowNew`/`Select`/`Next`/
  `Kill`/`Rename`), pane ops (`PaneSplit`/`Resize`/`Kill`/`Swap`), forge
  resume (`ForgeList`/`ForgeListOk` + `SessionsCreate{forge_session}`).

Ordering suggestion: Agents → Editor → Workflows → Triggers → Machines,
since Agents/Editor are used most.

## Phase 5 — polish + optional true-push

- **FCM (optional).** Add Firebase Messaging as a belt-and-suspenders
  channel *in addition to* the foreground service: daemon (or a Supabase
  edge function) posts to FCM on agent turn-end/question; the service
  forwards to a local notification. Needs a Firebase project + a push path.
  The foreground service already covers the common case; FCM only helps
  when the service itself is killed. Do this **only** if the service proves
  flaky on the target devices.
- **Google-OAuth login.** Today it's email/password (GoTrue). Add
  `expo-auth-session`-equivalent via `AppAuth`/`SignIn` if a Google account
  is preferred. Low priority; password works.
- **App icon / branding.** Proper launcher icon + adaptive icon + a real
  notification small-icon (currently a placeholder).

## Phase 6 — retire the RN app

Only after Phases 3–4 are verified on-device:
- Confirm every RN screen has a working native equivalent (parity matrix).
- Remove `mobile/` (keep the RN `sgr.ts`/`highlight.ts` as reference if
  ever needed, or delete).
- Update root `AGENTS.md`, `docs/SPEC.md` (client surface), and the build
  log; drop RN from the build/release docs.
- Decide whether the web client (`web/`) is in scope for the same
  treatment or stays as-is.

## Cross-cutting

- **Testing.** The two protocol round-trip scripts (local-socket PTY echo;
  Realtime join→attach→chunked-snapshot) are the regression net. Extend
  them per new frame family in Phases 3–4 before writing the UI. There is
  no on-device UI test in the repo yet — Phase 3 is the point where an
  emulator smoke test (attach a shell, type, see echo) should be added.
- **No daemon changes expected.** Every frame the native app needs already
  exists in `ranch-protocol`. If a gap is found, fix it in the protocol
  crate first and keep all clients in lockstep.
- **Build/sign.** `cd mobile-native && ./gradlew assembleRelease`
  (AGP 8.12.0, Gradle 9.4.1, Kotlin 2.2.10, SDK 35, JDK 17), signed with the
  shared `ranch` keystore. `applicationId` is `dev.ranch.android`
  (coexists with RN `dev.ranch.app` until Phase 6).
