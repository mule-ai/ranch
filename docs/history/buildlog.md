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
