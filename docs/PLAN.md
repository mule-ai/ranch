# Ranch — Implementation Plan

Companion to [SPEC.md](SPEC.md) (target system). This plan is grounded
in the code as of 2026-09-12: file paths, existing patterns to reuse,
and the forge/mule surfaces we build against. Milestones are sequenced
so each lands a usable increment; all three clients (TUI, web, mobile)
reach parity within each milestone or in its follow-up.

Conventions inherited from the build so far (AGENTS.md):

- One frame schema (`crates/ranch-protocol`) for unix socket + relay.
- Daemon workers get their **own pipe pair** (`relay::make_pipes`);
  worker → clients is broadcast like any client frame
  (`daemon.rs:handle_frame` match arms).
- Errors are `Frame::Error {req_id, message}`; requests correlate by
  `req_id`; failed agent splits must flash in the status bar, never
  `die()` the TUI (M8.4 pattern).
- Hot upgrade (Tier 2) must keep working: every new long-lived worker
  or child must be restartable across execve (threads die with the
  exec and reconnect; fds are CLOEXEC-cleared only when intended).

---

## Phase A — Agent tools (ranch is for agents)

The highest-leverage feature: agents that can spawn, steer, and reap
panes. Everything else (workflows, webhooks) composes with it.

### A1. Daemon-side agent session registry — `crates/ranch/src/agenttools.rs` (new)

- Track **spawn ownership**: `{spawned_pane → caller_pane, spawn_id,
  created_at}` in the daemon (`Daemon` struct gets `agent_spawns:
  HashMap<Uuid, SpawnRecord>`); persisted to `state.json` so callbacks
  survive Tier-1 restore.
- Surface: an internal API `spawn_agent_agentpane(kind, profile_or_pi,
  prompt, cwd, anchor_session) -> (session, pane)` that reuses the
  existing creation paths — `spawn_pane()` +
  `create_chat_pane()`/`LocalPi::spawn()` for kind="pi",
  `ForgeJob::Send` + `POST /sessions` for kind="forge" (see
  `daemon.rs:spawn_pane` ~L711 and `daemon.rs:865-855` for the two
  existing pane factories).
- Completion detection: reuse the exact signals the workers already
  emit — forge worker's `turn_ended` SSE handling (`forge.rs:run_sse`
  → `meta kind="agent" status="idle"`) and pi-local's `agent_end` /
  `turn_end` RPC mapping (`pilocal.rs:run_pi_reader`). A pane's
  transition working→idle is the callback trigger.

### A2. Frames — `crates/ranch-protocol/src/lib.rs`

```
AgentSpawn { req_id, caller_session, caller_pane,
             kind: "pi"|"forge", profile_id?, name?, cwd?,
             prompt, mode: "split"|"session",
             callback: bool }        → AgentSpawnOk {req_id, session, pane, spawn_id}
AgentSend  { session, pane, text, delivery: "steer"|"queue", req_id }
AgentStatus{ pane, req_id }           → AgentStatusOk {req_id, pane, state, busy, model?}
AgentRead  { pane, since_seq?, limit, req_id } → AgentReadOk {req_id, pane, msgs: Vec<ChatMsg>}
AgentClose { session, pane, req_id }  → AgentCloseOk {req_id}
AgentDone  { spawn_id, pane, session, outcome: "completed"|"failed"|"closed",
             last_row: ChatMsg? }     // daemon → caller pane (broadcast)
```

- `AgentSend` maps to forge `POST /messages` (forge worker already has
  `ForgeJob::Send`, `forge.rs:377`) or pi RPC `{"type":"steer"…}` /
  `{"type":"follow_up"…}` (pi 0.85.1 rpc.md — `steer` queues
  mid-turn, `follow_up` queues post-turn; `pilocal.rs` gains
  `steer()`/`follow_up()` next to `prompt()` at L350).
- `AgentClose` reuses `remove_pane_everywhere` + `LocalPi::kill()`
  (`pilocal.rs:371`), restricted to panes in `agent_spawns` (or the
  human's own panes are unaffected — agents can only close what they
  spawned).

### A3. Tool exposure to agents (topology matters)

The agent runtime is pi *or* forge, and forge usually runs on a
different host than ranchd (the lab deployment). Two transports, one
rule: **tool calls flow daemon-initiated** — ranchd has no inbound
listener, and a forge sandbox cannot reach ranchd's loopback.

- **Local pi panes**: a pi extension (`tools/ranch-pi-ext/`) —
  registered via `pi.registerToolProvider`, the same mechanism forge's
  own `forge-tools` extension uses (`extensions/forge-tools/`) —
  exposes `ranch_spawn`/`ranch_send`/`ranch_status`/`ranch_read`/
  `ranch_close` and forwards them to a **loopback HTTP control API on
  the daemon** (127.0.0.1 only, bearer token in env
  `RANCH_CONTROL_TOKEN` passed to the child; `pilocal` spawns with the
  extra env). The daemon endpoint is 5 routes that construct the
  corresponding `Frame` and enter the same `handle_frame` path clients
  use — one implementation of the semantics, two entry doors.
- **Forge panes (remote host)**: forge hosts the bridge. A
  `ranch-tools` pi extension (sibling of `forge-tools`) registers the
  same five tools in the forge-sandboxed agent, but they forward to
  **forge**, which **queues** the request and publishes a
  `ranch_tool_request` event on the session's existing SSE stream.
  ranchd's forge worker (already consuming that stream per watched
  pane) picks it up, executes the tool in `agenttools.rs`, and POSTs
  the result back to forge (`POST /ranch-tools/{id}/result`,
  authenticated with the forge API key it already holds; forge
  verifies session ownership/tenancy). forge's `/tools/execute`
  handler long-polls the pending row (bounded ~60 s) so the agent's
  turn stays synchronous. Forge changes: F3a (pending queue + SSE
  event + result endpoint + long-poll) and F3b (the `notify` hook for
  callbacks).
- **Callbacks**: on working→idle transition of a pane with a
  `SpawnRecord`, the daemon (a) broadcasts `AgentDone` to the caller's
  pane (renders as a system chat row: `✓ sub-agent "refactor" finished`),
  and (b) wakes the caller's harness — loopback long-poll
  (`GET /agent/callbacks?pane=…`) for local-pi callers; the forge
  bridge result path (F3b resolves the pending callback into the
  caller's session) for forge callers. The caller never polls.
- **Mule agents**: no ranch tools in v1 (mule has its own
  orchestration model; its workflow agents run on mule's host). The
  forge bridge pattern applies unchanged if we ever want it.

- **Policy**: `daemon.toml` `[agents] spawn_policy = "allow"|"ask"|
  "deny"`; `ask` emits an `AgentSpawnRequest` broadcast; any attached
  client can approve (new frame `AgentSpawnApprove {spawn_id, allow}`)
  — render as a chip in TUI/web/mobile chat input areas.

### A4. Client surfaces for A1–A3 (parity)

- All three clients render `AgentDone` rows in chat panes (already
  just chat rows — minimal work).
- Spawn-approval chips (when policy = ask): TUI status-bar prompt,
  web + mobile inline chip on the session.
- A "spawned by agent" badge on pane borders/title (PaneMeta gains
  `spawned_by: Option<String>`).

**Acceptance**: from an agent pane, `ranch_spawn` a pi sub-agent with
a task; a new pane appears in the same session for the human; the
sub-agent completes; the caller receives the callback row and reads
the result via `ranch_read`; `ranch_close` removes the pane. Human
can interject by focusing the pane and typing (it's just a chat pane).

---

## Phase B — Agent builder (ranch enables humans to build agents)

### B1. Forge-side prerequisites (../forge)

- **F1. Profile CRUD is already real** (`api/profiles.rs`:
  `POST /profiles`, `GET /profiles`, `PATCH /profiles/update`,
  `DELETE /profiles/delete|:id`) — ranch proxies it; no forge change
  needed for the MVP builder. Provider values are allowlisted
  (`ALLOWED_PROVIDERS` + the DB CHECK in migration 005: openai,
  anthropic, proxy-anthropic, proxy, google, gemini, custom); the
  builder's provider dropdown enforces the same set client-side.
- **F2. Model catalog** already exists (`GET /v1/models/catalog`,
  `api/openai.rs`) — used by ModelList today; reuse.
- **F3. The forge bridge (from A3, the remote-host story)** — two
  additive pieces:
  - **F3a — ranch-tool relay**: a pending-request queue for the
    `ranch_*` tool names + a `ranch_tool_request` event on the
    session's SSE stream + `POST /ranch-tools/{id}/result` (tenancy
    checked) + a bounded long-poll in the `/tools/execute` handler
    for `ranch_*` calls so the agent's turn stays synchronous.
  - **F3b — caller-notification hook**: `POST /sessions/{id}/notify`
    — persists a `role:"system"` row + publishes to the bus so an
    idle forge agent's harness can pick up a spawn callback. Small,
    additive (mirrors `messages.rs` create + bus publish).
- **F4. Agent recipes** (target-system feature): a `recipes` table +
  CRUD (`profile_ref`, `skills`, `workflow_hint`) — schedule with forge
  maintainers; the ranch side should code against a thin proxy so
  recipes can start as ranch-local JSON (`daemon.toml` or a
  `recipes/` dir) if forge lands late.

### B2. Frames + worker — ranch-protocol + forge.rs

- Frames: `ProfileList/ProfileListOk`, `ProfileGet/GetOk`,
  `ProfilePut {…profile fields…}/PutOk`, `ProfileDelete/DeleteOk`,
  `SkillList/SkillListOk` (forge skills surfacing is B3; mule skills
  have their own frames in Phase C).
- `forge.rs` worker gains job kinds alongside the existing
  `ForgeJob::{Watch, Send, List, ModelList, ModelSet}` (~L74-101):
  `ProfileList`, `ProfileGet`, `ProfilePut`, `ProfileDelete` — plain
  blocking ureq calls in the same worker thread pattern.
- Secrets: `api_key` in `ProfilePut` is accepted but never returned;
  `ProfileGetOk` carries forge's redacted view.

### B3. Client surfaces (parity)

- **TUI**: `:agents` opens the agent manager overlay (pattern: the M4.2
  sidebar) — profile list, edit form fields rendered as labeled input
  lines, `j/k` navigation. Model picker reuses the ModelList catalog.
- **Web**: `Agents` page in WebApp.tsx (same page pattern as the
  session dashboard) — form with model `<select>` from the catalog,
  system-prompt textarea, tools checkboxes, save.
- **Mobile**: Agents screen (pattern: Machines.tsx + Editor.tsx) —
  profile list → editor form; write-only API key field.
- All three: "new agent pane from profile" action (existing
  `SessionsCreate kind:"forge"` + `forge_session` adoption covers
  launching; builder just needs to expose profile ids).

**Acceptance**: on the phone, create a profile (name/provider/model/
system prompt/tools), then from the TUI create an agent pane using
that profile; the pane binds to a forge session running the profile's
model + prompt.

---

## Phase C — Workflows (mule) in ranch

### C1. Mule worker — `crates/ranch/src/mule.rs` (new)

Mirror the forge worker architecture (blocking HTTP in a worker thread,
own pipe pair, job enum + `mpsc`):

```rust
enum MuleJob {
    ListWorkflows, GetWorkflow(id), PutWorkflow(WorkflowDraft),
    DeleteWorkflow(id),
    ListAgents, ListSkills, ListProviders,      // editor pickers
    Run { workflow_id, input, pane: Uuid },      // opens the stream
}
```

- REST against `../mule/cmd/api/server.go` routes: `/api/v1/workflows`
  (+`/{id}`, `/{id}/steps`, `/steps/reorder`), `/api/v1/agents`,
  `/api/v1/skills`, `/api/v1/providers`, `POST /api/v1/jobs`
  (`createJobHandler` takes `{workflow_id, input_data,
  working_directory}`) — note **mule currently has no auth middleware
  on these routes** (`server.go:140-142` mounts logging/recovery/CORS
  only); config carries `mule_url` and an optional `mule_api_key`
  header for when mule grows auth (coordinate upstream).
- Run streaming: mule's WS hub (`/ws`) broadcasts `WebSocketMessage
  {type, data, timestamp}` with types `job_update`, `job_step_update`,
  and agent event types (`BroadcastAgentEvent`) — **global firehose,
  no per-job filter** (websocket.go:120-147). The worker: connects
  once, filters client-side by `job_id`, formats rows into the
  workflow pane's VT (`vt.write` — a workflow pane is a headless pane:
  VT but no PTY, rows written by the worker; same trick forge chat
  panes use for the relay pipe).
- Status: `Meta {kind:"workflow", status, job_id}` on job_update
  transitions (`QUEUED→RUNNING→COMPLETED|FAILED`, `pkg/database/
  models.go:86-95`).

### C2. Frames — ranch-protocol

```
WorkflowList/ListOk, WorkflowGet/GetOk, WorkflowPut {draft}/PutOk,
WorkflowDelete/DeleteOk,
WorkflowRun {workflow, input?, req_id} → WorkflowRunOk {req_id, job, session, pane}
```
(+ `AgentList/ListOk`, `SkillList/ListOk`, `ProviderList/ListOk`
namespaced as `MuleAgentList` etc. to avoid colliding with forge
profile frames.)

### C3. Client surfaces (parity)

- **TUI**: `:workflows` overlay — list, `r` run (input prompt), `e`
  edit (step list + JSON config editor), `n` new. Workflow pane is a
  normal terminal pane (scrollback, splits, detach-safe).
- **Web**: Workflows page — list + step editor (drag ordering can
  wait; reorder endpoint exists) + run button → navigates to the pane.
- **Mobile**: Workflows list screen; run → auto-attach to the
  workflow pane; step status rendered as timeline cards from
  `job_step_update` metas.

**Acceptance**: create a 2-step workflow (agent → agent) from the web
app; run it from the phone; watch both steps stream live in a pane;
job completes; status badge updates on all clients.

---

## Phase D — Triggers (cron + event)

### D1. Trigger scheduler — `crates/ranch/src/triggers.rs` (new)

- Registry: `Vec<Trigger>` in the daemon (persisted to `state.json`,
  mirrored to Supabase `triggers` table for dashboards).
- **Cron**: parse via a minimal cron crate (add dependency; keep
  musl-static compatible), tick in the daemon's existing poll loop
  (check due triggers each tick — the loop already runs at 30 ms; a
  1 s resolution sub-tick is enough), honor `tz`.
- **Event hooks**: the daemon already sees every interesting event
  internally — wire trigger evaluation at the existing emit points:
  `meta kind:"agent" status:"idle"` (agent turn ended, forge.rs:2138
  area), workflow pane status transitions (C1), `FileChanged` watch
  hits (daemon.rs M10 phase-3 watch loop), pane exit (the existing
  dead-pane cleanup). Triggers carry a filter
  `{event_type, session?, pane?, workflow?, payload_glob?}`.
- **Webhook triggers**: evaluated on `WebhookEvent` frames (Phase E).
- Fire: `POST /api/v1/jobs` via the mule worker + record
  `last_run` on the trigger + broadcast `TriggerFired {trigger, job}`.
- Missed cron runs while offline: optional `catch_up: bool`
  (default false) — on boot, if `last_run` is older than the previous
  due time and catch_up is set, fire once.

### D2. Frames

```
TriggerList/ListOk, TriggerPut {trigger}/PutOk, TriggerDelete/DeleteOk,
TriggerRun {trigger, req_id}/TriggerRunOk, TriggerFired {trigger, job}
```

### D3. Client surfaces (parity)

- Triggers section in the workflow screens (all three clients):
  list with next/last run, enable/disable toggle, cron field with
  plain-text preview ("0 3 * * *" → "daily at 03:00"), event picker
  (dropdown of event types + id pickers), webhook binding (from E).

**Acceptance**: create a cron trigger (nightly) and an event trigger
("when workflow X completes, run workflow Y" — a mule-native chain);
both fire without any client attached; dashboard shows last-run
status; disabling stops firing.

---

## Phase E — Webhook receiver (relay)

### E1. Supabase side — `supabase/migrations/0007_webhooks.sql`,
`supabase/functions/webhook-relay/` (new edge function, TypeScript)

- Migration: `webhooks` + `webhook_log` tables + RLS (SPEC.md §9,
  full design in docs/design/webhook-receiver.md).
- Edge function (`webhook-relay/w/<webhook_id>`):
  1. lookup webhook (service role), 404 on unknown id;
  2. verify HMAC (`X-Ranch-Signature: t=…,v1=…`, constant-time,
     ±5 min freshness) → 401;
  3. rate-limit check (token bucket in the function's shared state or
     a `webhook_log` count query) → 429;
  4. cap body at 64 KB → 413;
  5. broadcast a `WebhookEvent` frame to
     `realtime:machines:<machine_id>` with the service role;
  6. insert `webhook_log` row; 202.
- Deploy with `supabase functions deploy webhook-relay`.

### E2. Daemon side — `crates/ranch/src/webhooks.rs` (new)

- Webhook CRUD proxy: frames `WebhookList/WebhookPut/WebhookDelete`
  → REST (PostgREST) against the `webhooks` table with the machine's
  JWT (RLS allows the machine user row access) or the owner JWT via
  the relay token — reuse the `relay.rs` REST helpers
  (`mirror_upsert` pattern at relay.rs:271).
- Secret generation: `WebhookPut` returns the raw secret **once** (the
  client shows it with copy affordance); only the hash is stored.
- `WebhookEvent` frames arrive on the relay pipe (already the path for
  every remote frame — `handle_ws_text` at relay.rs:557 routes
  broadcasts into the daemon poll loop); daemon matches them against
  triggers (D1) and fires workflows. No trigger match → log only.

### E3. Client surfaces (parity)

- Webhooks section (workflow screens): list (name, machine, source
  tags, last received), create (name + machine + sources), the
  one-time secret + URL display, copy button, test-fire (`curl`
  snippet in a details drawer).

**Acceptance**: create a webhook trigger bound to workflow "triage";
`curl -X POST <url> -H "X-Ranch-Signature: …" -d '{"alert":…}'` →
workflow runs on the machine; wrong signature → 401; log shows the
attempt; second user's webhook cannot target my machine.

---

## Phase F — TUI file browser/editor (parity closure for §4)

- `:files [dir]` overlay in the TUI over the existing
  `DirList/FileRead/FileWrite/FileChanged` frames — nothing new on the
  wire. Read-only viewer with inline edit buffer + Save (mtime
  conflict UX identical to mobile). Full editing stays delegated to
  `$EDITOR` in a shell pane (`prefix-E` opens the focused file in
  `$EDITOR` in a split) — the multiplexer stays the multiplexer.

---

## Sequencing & dependencies

```
A (agent tools) ──▶ C (workflows) ──▶ D (triggers) ──▶ E (webhooks)
        │                                     ▲            │
        └── B (agent builder) ────────────────┘            │
              (B is independent; D's event triggers        │
               consume A's spawn events + C's run events) ◀┘
F (TUI files) — independent, slot anywhere
```

- A first: it's the differentiator and everything composes with it.
- B in parallel (mostly client work + thin proxies).
- D depends on C (fires workflows) and A (event sources).
- E depends on D (webhook triggers are the consumer).

## Testing posture (per repo conventions)

- Protocol: unit tests in `ranch-protocol` for every new frame family
  (serde round-trip + chunking).
- Daemon: socket-level tests driving real frames (pattern: M4.1 swap
  tests, M5 window tests in `daemon.rs` test module) — spawn steering
  and close need a fake `pi --mode rpc` child (the existing pi tests
  already stub rpc I/O).
- Webhooks: signature verification vectors + stale-timestamp rejection
  unit tests; Realtime broadcast mocked behind the relay pipe.
- TUI: PTY-driven tests (pattern: M2.6/M4.2) for the new overlays.
- Mobile/web: manual parity checklist per milestone (the repo has no
  JS test rig; keep it that way for now).

## Definition of done (per feature)

- Shipped on all three clients (or explicitly daemon-side).
- Frames documented in PROTOCOL.md; spec §(2/5/6/7) updated.
- Hot-upgrade safe (workers restart, no pane death, `ranch upgrade`
  e2e pass).
- `make test` + `make lint` green; static build unaffected (~21 MB
  budget).
