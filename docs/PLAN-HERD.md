# Herd — Implementation Plan

The build plan for the Herd initiative: **many always-on, always-learning
agents, self-hosted, on our own infra.** This document is the working
instruction set for the agents doing the implementation. Every task
names its repo, the files it touches (with line anchors as audited on
**2026-10-06**), what to build, and how to verify it.

Source of truth for the *concept*: `docs/research/always-on-agents-research.md`
(read it first; this plan operationalizes it). Where this plan and the
research doc disagree, this plan wins unless it says otherwise.

## 0. Conventions for the implementing agents

### Repos

| Repo | Path | Language | Role after Herd |
|---|---|---|---|
| forge | `~/src/forge/` | Rust (axum, sqlx, tokio) | Hosts agents' lives: agent registry, durable conversations/tasks, memory tables, voice proxies, sandbox |
| mule | `~/src/mule/` | Go | Behavior plane: wakes, reflection/behavior workflows, policy engine, async glue |
| ranch | `~/src/ranch-2/` (this repo) | Rust | Presence: panes, agent registry surface, activity view, approvals, voice I/O, triggers, mobile/TUI/web |
| pi-durable | `github.com/earendil-works/pi` → `packages/durable`; npm `@earendil-works/pi-durable@1.0.4` | TypeScript | **The durable runtime itself** (decided): forge's harness runs it; we contribute the Postgres backend + conformance validation |

The one-sentence division of labor: *forge hosts the agents' lives,
mule scripts their behavior, ranch is everywhere they can be seen and
heard.*

### Ground rules (non-negotiable)

1. **Nothing is "later".** Every capability in the research doc gets a
   task in this plan. If a task is genuinely huge, it gets split, not
   deferred.
2. **Docs move with behavior.** Each phase's "docs" task (§10) is part
   of the phase, not optional. Touch: forge `docs/ARCHITECTURE.md`,
   `docs/API.md`, `docs/SCHEDULED-AGENTS.md` (replaced), mule
   `spec.md`/`plan.md`, ranch `docs/SPEC.md`, `docs/PROTOCOL.md`,
   `mobile-native/README.md`, and this file's phase status.
3. **Ranch invariants** (from AGENTS.md): server-side ghostty-vt
   emulation only; one frame schema on both transports; workers get
   their own pipe pair; bounded non-blocking writes to worker pipes;
   hot-upgrade (Tier 2) must survive every new worker; session death
   is `panes.is_empty() && chats.is_empty()`; pi errors surface as
   `⚠` chat rows.
4. **Feature parity**: every user-facing capability lands on TUI, web
   (`web/`), and mobile-native (`mobile-native/`) in the same
   phase, or is explicitly daemon-side.
5. **Forge invariants**: Postgres is the source of truth; tenancy
   (users + SHA-256 API keys + admin, `src/api/auth.rs`) is preserved
   through the refactor; SSE event names consumed by ranchd
   (`turn_ended`, `ranch_tool_request`, `agent` meta) keep working or
   are migrated atomically with the ranchd side; the OpenAI surface
   (`/v1/chat/completions`, `model: "forge:<uuid>"`) and voice proxies
   (`/v1/audio/*`) keep working.
6. **Verification per task**: run the repo's test suite (forge
   `just test` + integration, mule `make test`/`go test ./...`, ranch
   `make test` + `make lint`), and for user-visible tasks run the
   phase's demo checklist (§10.3).
7. **Line anchors drift.** All `file:line` references in this plan are
   as of 2026-10-06. Re-anchor with grep when you start each task; the
   *symbol* names are the stable references.

### Deployment topology (audited 2026-10-06; user directive: **mini hosts
**everything, as Apple Linux containers)**

- **`mini`** (macOS M4, `mini.botnet`, 10.10.199.29) is the canonical
  host for the whole stack. Services run as **Apple Linux "machine"
  containers** (`com.apple.container`, ALC): aarch64 **NixOS VMs**
  whose configuration lives in `~/src/lab/ct/<name>/`
  (`configuration.nix` + `flake.nix`), images are built into
  `/var/lib/lab-alc/images/`, and machines are created/recreated by
  `sudo lab-alc-reconcile` on mini. Machine lifecycle gotchas are
  documented per-machine in the `lab/ct/<name>/README.md` files
  (read `lab/ct/forge/README.md` before any rebuild).
  - **`forge`** machine (`forge-461980`, image `local/forge-alc:latest`):
    `forge-api` systemd service inside the VM (:8080, exposed on mini
    as `http://10.10.199.29:8110`), **PostgreSQL 16 inside the same
    VM** (db `forge`, trust auth on localhost —
    `lab/ct/forge/configuration.nix:44–47`), **pgvector enabled on the
    forge db (H0.4)**, **pi 0.84.4 and the
    forge-tools extension baked into the image** at build time, nspawn
    sandboxes under `/forge/sessions/` (Debian bookworm base rootfs).
    Secrets live on the macOS host at
    `/Users/jbutler/.lab-alc/data/forge/forge.env` (Bitwarden vault
    `forge`), mounted into the VM.
  - **`mule`** machine (private, the **fleet mule**; config
    `lab/ct/mule/`, added H0.4): mule service + PostgreSQL 16 inside the
    VM (db `mule`), exposed on mini as **`http://10.10.199.29:8091`**
    (VM :8080). Secrets at `/Users/jbutler/.lab-alc/data/mule/mule.env`
    (Bitwarden vault `mule`): `MULE_ENCRYPTION_SECRET`, `DATABASE_URL`,
    `FORGE_URL` (= `http://10.10.199.29:8110`), `FORGE_API_KEY`.
    Egress: loopback, forge, bifrost (10.10.199.29:8080), lab DNS. See
    `lab/ct/mule/README.md` for the build/reconcile runbook + the
    consumer repoint checklist (omarchy daemon.toml `mule_url`,
    ranch-mini, ranch-demo).
  - **`mule-public`** machine: the *public demo* mule
    (`mule.jbutler.dev`, `lab/ct/mule-public/`). **Not the fleet
    mule**; untouched by Herd.
  - Other machines: `bifrost` (inference router; Qwen3-Embedding-4B /
    Qwen3-Reranker-4B, config `~/src/lab/ct/embeddings/` +
    `ct/bifrost`), `voice-*` (Parakeet STT + Kokoro TTS), `searxng`,
    `matrix`, `ha`, `ranch-mini`, `ranch-demo`, `mule-builder`, etc.
- **`omarchy`** (this box, 10.10.199.177): ranch-2 development + the
  user's own registered ranchd (`~/.config/ranch/daemon.toml` →
  Supabase project `prqfseydoxyingbkmiic`, forge at
  `10.10.199.29:8110`, `mule_url` `127.0.0.1:8080` — **stale; H0.4
  repoints it at the new `mule` machine**).
- **`ha`**: NOT part of this project. (The audit's early "ha = forge
  host" finding came from flaky DNS for `ha.botnet` pointing at a
  stale staging box; the user confirmed ha is not a Postgres host.)
  Ignore `ha` in all Herd work.
- **Supabase** (`prqfseydoxyingbkmiic`): the cloud layer (Realtime
  relay, `webhook-relay` edge function, registry mirror tables
  `supabase/migrations/0001`–`0007`). Stays in the cloud; "everything
  on mini" applies to the self-hosted services.
- **pi versions**: this box runs pi `1.0.4` (mise); the **forge image
  bakes pi 0.84.4** — H0.5 reconciles on one pin. Forge's extensions
  declare `"@mariozechner/pi-coding-agent": "*"`
  (`extensions/forge-tools/package.json:20`) — H0.5 tightens all pi
  pins. **H0.5 outcome (2026-10-06): the herd pin is pi `1.0.4`.**
  Forge extension peerDeps now pin `1.0.4`; verified against the local
  1.0.4 binary: the full spawn-flag set (`--mode rpc`,
  `--no-builtin-tools`, `--no-extensions`, `-e/--extension`,
  `--no-skills/--skill`, `--no-prompt-templates`, `--provider`,
  `--model`, `--thinking`, `--no-session/--session`) and the no-key RPC
  failure envelope (`{"type":"response","command":"prompt",
  "success":false,"error":"No API key found …"}`) are unchanged from
  0.84.4 — forge `pi_agent.rs` and mule `pirc/pibridge.go` work as-is
  (mule `go test ./...` green; forge workspace 251 tests green). The
  forge image (`lab/ct/forge/flake.nix`) and the new fleet mule image
  (`lab/ct/mule/flake.nix`) still bake 0.84.4 — bump both to 1.0.4 at
  the next image rebuild (H0.4 reconcile); ranchd warns at boot on
  drift (`pi_pin_warning` in `crates/ranch/src/daemon.rs`).

---

## 1. Current-state audit (ground truth this plan builds on)

### 1.1 Forge (`~/src/forge/crates/forge-api/`, ~20k LOC)

Schema today (migrations `001`–`015`, `crates/forge-api/migrations/`):

- `profiles` (001): model/provider/base_url/api_key, `working_dir`,
  `git_url`, `system_prompt`, `tools` JSONB list.
- `sessions` (001): `profile_id`, `title`, `cell_host`/`cell_state`
  (nspawn), `last_active`; +006 `override_provider/model/base_url/api_key`;
  +014 `working_dir` (anchor mode for ranch agent panes).
- `messages` (001): `session_id`, monotonic `sequence`
  (UNIQUE(session_id, sequence), allocated by `get_next_sequence`
  PL/SQL fn, locking variant added in 004), `role`
  (user/assistant/tool/system), `content`, `tool_name`, `tool_input`,
  `tool_call_id`; +003 `tool_output`.
- `users` / `api_keys` (002): argon2 `password_hash`, `role`
  ('admin'|'user'), SHA-256 `key_hash` + `key_prefix`; +015
  `restricted` flag (no tools / no profile CRUD / no working_dir).
- `session_embeddings` (007): `REAL[]` embedding + `summary` per
  session (in-app cosine — no pgvector anywhere yet).

Module map (files are `src/` of `crates/forge-api`; sizes in lines):

| File | What it owns |
|---|---|
| `api/mod.rs` (1133) | `AppState`; `dispatch_message` L294 (INSERT user row → bus publish → `agent_registry.get_or_create` → tokio-spawned `drive_turn` → background `refresh_session_summary`); `execute_tool` L439 (HTTP `/tools/execute` handler); `create_router` L923 (full route list L924–1015); `build_app` L1069 |
| `api/turn.rs` (594) | `drive_turn` L191 / `drive_turn_with` L238 over a `PiEventSource` trait (implemented by `PiAgent` at `pi_agent.rs:783`); `TurnEndReason` L87 |
| `api/messages.rs` (99) | `GET/POST /messages` (`session_id` in body/query); tenancy gate `session_access_err` L26 |
| `api/sessions.rs` (928) | create/list/get/delete/sever; `/context` L956, `/compact` L957, `/interrupt` L958 |
| `api/sse.rs` (1554) | SSE streaming; `api/events.rs` (502): `GET /sessions/:id/events` stream the ranchd forge worker consumes (`ranch_tool_request` events included) |
| `api/ranch_tools.rs` (327) | F3a/F3b bridge: `RanchToolQueue` L66, `relay_ranch_tool` L117, `session_notify` (F3 `notify` hook), `/ranch-tools/:id/result` |
| `api/routing.rs` (1383) | `POST /router/message` L179: embed → top-K → rerank → route to session or start new; `refresh_session_summary` |
| `api/openai.rs` (1472) | `/v1/chat/completions`; `model:"<profile>"` stateless vs `model:"forge:<uuid>"` stateful (sentinel L100–101); `/v1/models` |
| `api/voice.rs` (471) | `POST /v1/audio/transcriptions` L128 (→ `PARAKEET_URL`), `/v1/audio/speech` L203 (→ `KOKORO_URL`), `/v1/audio/voices` L267 |
| `api/auth.rs` (1245) | register/login, API key CRUD, `can_access` tenancy, admin bootstrap |
| `api/profiles.rs` (481), `api/admin.rs` (368) | profile CRUD; sandbox reset, session-replay admin endpoint |
| `agent_registry.rs` (923) | `AgentRegistry` L130: one `SharedPiAgent` per session; `get_or_create` L338 (spawn + durable-resume path); `begin_turn` L767/`end_turn` L775; `tool_auth_token` L334 |
| `pi_agent.rs` (823) | `PiAgent` L250: `pi --mode rpc` subprocess; `spawn` L258, `send_message` L497, `send_tool_result` L505, `compact` L550, `read_line` L626, `PiEvent` enum L97 |
| `tool_executor.rs` (1669) | `ToolExecutor` L192 / `execute` L295: bash/read/write/edit in the nspawn sandbox; the HTTP entry `execute_tool` L1161 |
| `recording.rs` (199) | `DbToolRecorder` L111 (dual-writer: harness writes call rows, executor writes result rows) |
| `resume.rs` (544) | **filesystem replay**: re-execute prior bash/write/edit to rebuild the sandbox working tree on resume |
| `session_replay.rs` (996) | **context replay**: rebuild a pi session jsonl from `messages` for `new_session`/`parentSession` |
| `session_manager.rs` (247) | per-session dirs/state on disk; idle bookkeeping (30-min `SESSION_TIMEOUT_SECS`, `lib.rs:46`; cleanup task `lib.rs` `cleanup_task`) |
| `bus.rs` (270) | in-process tokio broadcast: `BusEvent` L53, `publish_turn_ended` L136, `publish_ranch_tool_request` L146 |
| `embedding.rs` (395) | `embed` L71 / `rerank` L128 via Bifrost (Qwen3), `cosine_similarity` L231 (in-app, `REAL[]`) |
| `sandbox.rs` (1165) | `SandboxManager`: systemd-nspawn per-session containers, rootfs/clone-from-git, pinned toolchain |
| `lib.rs` (333) | `run()`: env (`DATABASE_URL` L53, `FORGE_API_URL`), migrations, admin bootstrap, cleanup + metrics background tasks, `AppState::new` |

Turn flow (the thing the refactor replaces): `POST /messages`
(`api/messages.rs:85` `create_message`) → `dispatch_message`
(`api/mod.rs:294`): single-statement `INSERT … get_next_sequence()`
user row → `bus.publish_message` → `touch_session` →
`agent_registry.get_or_create` (spawns/reuses the pi subprocess; on a
cold session runs `resume.rs` filesystem replay + `session_replay.rs`
jsonl) → heuristic 300k-token compaction check (`api/mod.rs:349–361`) →
`tokio::spawn(drive_turn)` (`api/turn.rs:191`): event loop over pi RPC
(`PiAgent::read_line`), assistant/tool events written to `messages`
with sequence allocation, tool calls **forwarded to
`POST /tools/execute`** (the `forge-tools` TS extension in
`extensions/forge-tools/` does the round trip; `tool_executor.rs`
executes in the sandbox; `recording.rs` writes the result row; result
returned to pi via `send_tool_result`) → on `agent_end`:
`turn_ended` bus publish (SSE `turn_ended` event → ranchd meta
`kind="agent"`), fire-and-forget `refresh_session_summary`
(`api/routing.rs`) for the semantic router.

Extensions: `extensions/forge-tools` (TS; registers bash/read/write/
edit that call back over HTTP with a tool auth token) and
`extensions/ranch-tools` (TS; the `ranch_*` tool set that flows through
`api/ranch_tools.rs`). Both are built to `dist/` (`just ext`).

### 1.2 Mule (`~/src/mule/`, ~14k LOC Go)

Schema (`internal/database/migrations/0001_initial_schema.sql` +
0002–0010): `providers`, `tools`, `agents` (config only: provider,
`system_prompt`, `tools` JSONB, `model_id`), `workflows`
(`is_async`), `workflow_steps` (`step_type CHECK IN ('agent','wasm_module')`
+ `config` JSONB), `wasm_modules`, `agent_tools`, `jobs`
(`queued|running|completed|failed|cancelled`, `input_data`/`output_data`
JSONB, `error_message`, working_dir from 0007), `job_steps`,
`artifacts`. Plus `memory_config` (0002: a genai-memory-tool config
row — **not** a memory store), `settings`, `wasm_module_sources`.
IDs are `VARCHAR(255)` (uuid strings).

Module map:

| File | What it owns |
|---|---|
| `cmd/api/server.go` (343) | route table: `/ws` L158; `/v1/models` + `/v1/chat/completions` L216–217; `/api/v1/{providers,tools,skills,memory-config,settings,agents,workflows,jobs}` CRUD L220–279+ |
| `cmd/api/handlers.go` (2031) | `chatCompletionsHandler` L133: `model:"agent/<name>"` → `runtime.ExecuteAgentWithWorkingDir`; `model:"async/workflow/<name>"` → job + `AsyncJobResponse`; `model:"workflow/<name>"` → sync if not async |
| `cmd/api/memory_handlers.go` | `GET/PUT /api/v1/memory-config` + `ReinitializeMemoryTool` |
| `internal/engine/engine.go` (498) | `Engine`: `SubmitJob` L93, `jobPoller` L124 (`SELECT … FOR UPDATE SKIP LOCKED` style polling), `worker` L158, `processJob` L181, step dispatch L357–496 (`processAgentStepWithWorkingDir` L369, `processWASMStepWithWorkingDir` L424) |
| `internal/engine/wasm.go` (2399) | wazero `WASMExecutor` |
| `internal/agent/runtime.go` (393) + `internal/agent/pirc/` | agent step runtime over pi: `pirc/pibridge.go` (622) `Bridge` L154 / `Start` L178 / `buildArgs` L250 (`pi --mode rpc`); `pirc/event_mapper.go` (403) |
| `internal/api/websocket.go` (323) | `WebSocketHub` L60: global firehose — `BroadcastJobUpdate` L123, `BroadcastJobStepUpdate` L137, `BroadcastAgentEvent` L151; `JobStreamer` L258. **No per-job subscription.** |
| `internal/api/middleware.go` (404) | logging/CORS/timeout/recovery/validation only. **No auth, no tenancy.** |
| `internal/primitive/store_pg.go` (917) | all CRUD against the schema above |
| `pkg/job/store_pg.go` (566) | job/job-step persistence |
| `internal/manager/` | workflow (433), provider, agent, skill, tool managers (UI-facing) |

Ranch talks to mule today via `crates/ranch/src/mule.rs` (551 lines):
`MuleConfig` L29 (`mule_url` from `daemon.toml`, optional `api_key`),
`MuleJob` enum L66 (`List/Get/Put/Delete/Run` → `POST /api/v1/jobs`
L503), worker thread L302 with its own pipe pair. The trigger
scheduler (`crates/ranch/src/triggers.rs`, 629) fires cron/event/
webhook triggers **machine-locally** and can submit mule jobs
(`Scheduler::new` L248 takes the `MuleJob` mpsc). Mule has **no**
cron/webhook/event machinery of its own — grep for `cron|webhook|
scheduler|timer|forge` in `*.go` (non-test) returns nothing.

### 1.3 Ranch (`~/src/ranch-2/`, this repo)

- Protocol: `crates/ranch-protocol/src/lib.rs` (2083). `Frame` enum
  L19. Relevant families: agent panes (`ChatSend` L489, `Chat` L514,
  `Meta` L598, `ChatCompact` L585), agent tools (`AgentSpawn…` /
  `AgentAsk` L735, `AgentAskRequest` L761, `AgentAskAnswer` L777,
  `AgentAskStatus` L787, `AgentAskStatusOk` L795), workflows
  L848–892, triggers L900–924, `WebhookEvent` L960. Planned families
  are spec'd in `docs/PROTOCOL.md` §8 (L407+).
- Daemon: `crates/ranch/src/daemon.rs` (6944). `spawn_pane` L1029;
  pane factories: pty L1152, forge-chat L1187 (via `forge.rs` worker
  `ForgeJob::Send`); local-pi panes via `pilocal.rs` (1784; `LocalPi`,
  reader L1769 meta `kind="agent"`). `state.json` restore (Tier 1) and
  `--inherit` hot upgrade (Tier 2) per AGENTS.md. `Meta` kinds today:
  `agent` (working/idle — `forge.rs:458,665`, `daemon.rs:3617`),
  `model` (`forge.rs:503,723`, `pilocal.rs:1733`), `context`
  (`forge.rs:890`, `pilocal.rs:1746`), `exited` (`daemon.rs:1988`),
  `workflow` (`mule.rs:273,536`).
- Agent tools: `agenttools.rs` (514): `SpawnRegistry` L89
  (pane→caller ownership, `authorized` L112), `AskRegistry` L337
  (`AskRecord` L301, first-answer-wins, TTL `prune` L357),
  `agent_done_row` L166 / `agent_done_frame` L418. Two transports:
  loopback control API (`control_api.rs`, 235; `RANCH_CONTROL_TOKEN`)
  for local-pi callers; forge-hosted bridge (forge `api/ranch_tools.rs`
  + SSE `ranch_tool_request`) for forge callers.
- Triggers: `triggers.rs`: `Trigger` L32 (cron/event/webhook),
  `Scheduler` L239, `tick` L303, `fire_event` L354,
  `cron_matches` L160. Webhooks arrive via the Supabase
  `webhook-relay` edge function → relay → `WebhookEvent` frame.
- Mobile-native (`mobile-native/`, Kotlin): `MonitorService.kt` (77)
  foreground service holding the Realtime socket; `Monitor.kt` (193)
  frame dispatch; `Notify.kt` (284): `onFrame` L77 → `onMeta` L115 /
  `onChat` L139 → pane-turn-end + `⚠`-error notifications,
  `postLocal` L247; `AgentsActivity.kt` (166) is a **local pi session
  manager** (PiList/adopt), not an agent registry; `SessionActivity.kt`
  (1860) terminal/chat panes; `Realtime.kt` (404) join/send;
  `Prefs.kt` (23).
- Web: `web/` (Vite + React, GitHub Pages). The old Expo web build of
  `mobile/` is gone — the RN app was deleted 2026-10-16.
- Cloud: `supabase/migrations/0001`–`0007` (machines, sessions mirror,
  Realtime RLS via `realtime.messages` probe, self-serve register,
  webhooks/triggers mirror), `supabase/functions/webhook-relay`.

### 1.4 Pi-durable (the semantic reference)

`@earendil-works/pi-durable@1.0.4` (npm), source
`github.com/earendil-works/pi` → `packages/durable` (TypeScript,
vitest; `src/{harness,session,storage,tasks,documents,env,testing,tools}`
+ `docs/spec.md`; tests in `test/harness-*.test.ts`). Audited 2026-10-06
from the npm package (`dist/types.d.ts`):

- **Storage interface** (`dist/types.d.ts:787`, `interface Storage`):
  `commit(writes, ctx) → Seq` (atomic batch), `mintId`,
  `conversation(id)` / `scanConversations`, `entry(id)` /
  `entry(conversationId, id)` / `findLatestHeadMarker` /
  `scanEntries`, `task(id)` / `scanTasks`, `submission(id)` /
  `scanSubmissions` / `submissionByRequest(conversationId, requestId)`
  (exactly-once dedup), `findDocument(address, point)` /
  `document(id, point)` / `scanDocuments`, `close`.
- **Records**: `ConversationRecord` L246 (agent config as a row:
  model, thinking, extension/tool names, instructions, cwd — names,
  not code); `EntryRecord` L272 (transcript entries; `head` marker
  semantics); `TaskRecordBase` L456 + `TaskState` L425 +
  `TaskOwnership` L218 (ownership tree; `background: true`);
  `SubmissionRecord` L318; `DocumentRecord` L486 with
  `DocumentSemantics` L43 (fork semantics: `asOf`/current/fresh).
- **Machinery**: `PhaseHandler` L119, `TaskRuntime` L132,
  `HookRunner` L121 (hook chains with memoized decisions);
  checkpoints per task step; `harness.resume()` finds unfinished
  tasks; interrupted tool calls rerun only when declared
  `replay: "safe"`; compaction is a background task; documents commit
  atomically with the transcript.
- **Conformance suite for backends**: `src/testing/
  storage-conformance.ts` (+ `runner.ts`, `assertions.ts`,
  `types.ts`; benchmarks `storage-benchmark.ts`) — a third-party
  storage backend implements `Storage`, runs the conformance suite,
  done. This doubles as our porting spec.

## 2. Target architecture

```
                       ┌──────────────────────────────┐
                       │  Supabase (cloud)            │
                       │  auth · relay · webhooks ·    │
                       │  agent registry mirror ·     │
                       │  push notifications          │
                       └──────▲───────────────┬───────┘
      TUI ──────┐             │ frames         │ webhook → relay
      Web ──────┼── clients ──┘                ▼
      Android ──┘        ┌──────────────────────────────┐
                         │ ranchd (user's machine)      │
                         │  panes · agent registry UI · │
                         │  activity view · approvals · │
                         │  voice I/O · triggers        │
                         └───┬──────────────────────┬───┘
                             │ REST/SSE             │ REST/WS
                  ┌──────────▼─────────┐    ┌───────▼──────────┐
                  │ forge (refactored) │    │ mule             │
                  │  agents entity     │    │  wakes           │
                  │  durable conversa- │    │  reflection /    │
                  │  tions + tasks     │◄───┤  behavior wfks   │
                  │  timers/hooks/docs │    │  policy engine   │
                  │  memory (pgvector) │    │  voice loop jobs │
                  └────────────────────┘    └──────────────────┘
                       Postgres (forge machine)     Postgres (mule machine)
                       — both ALC on mini           — ALC on mini
```

New concept at the center: **the Agent entity** (a "dot"):

- `agents(id, name, avatar, owner, home_machine, primary_profile,
  visibility, memory_scope, …)` in forge. Sessions/conversations are
  *conversations the agent has*; panes are *where humans watch*;
  workflows are *behavior mule scripts for it*. One agent, many
  conversations, many surfaces.
- Specialist dots = agent + credentials scope + tools allowlist + org
  memory read/write policy.

Phases (dependencies: H0 → H1 → H2 → {H3, H4} → H5 → H6; H3/H4 are
largely parallel, H5 consumes both, H6 needs H1+H4):

| Phase | Content | Repo(s) |
|---|---|---|
| H0 | Foundations: pi-durable Postgres storage backend + conformance, runtime decision record (pi-durable upstream), infra (pgvector, fleet mule machine), pins | forge, lab (mini infra) |
| H1 | Agent entity + registry; "talk to agent anywhere" | forge, ranch |
| H2 | Forge durable-core refactor onto the pi-durable harness (Node runtime + unix-socket IPC; tasks/checkpoints/ownership/hooks/docs/compaction come from upstream; deletes resume.rs/session_replay.rs/turn driver) | forge |
| H3 | Mule behavior plane: tenancy, `forge_conversation` step, wakes, per-job WS, policy engine | mule |
| H4 | ✅ complete | Memory & learning: episodes/beliefs (pgvector), episode capture, prompt sections, reflection workflows, belief-approval cards, cross-agent signals | forge, mule, ranch | H4.1 ✅ + H4.3 part 1 ✅ (forge `c9d4ee54` — `022_memory.sql`: episodes/beliefs/memory_acl/agent_signals + belief_audit, `embedding vector(2560)` — PLAN said 1024 but `embedding.rs` documents Qwen3-Embedding-4B as 2560-dim (corrected); HNSW cosine indexes; `src/memory.rs` typed store (episodes insert/search w/ provenance, belief lifecycle + version + belief_audit rows, memory_acl, agent_signals consumed_by) + 4 pure-Rust unit tests; API `GET /agents/:id/memory/search?q=&k=&scope=` (embed query → cosine, org beliefs gated by memory_acl) + `GET /agents/:id/memory/beliefs?status=` + `:bid`; `memory_remember(content, kind?)` forge-tool → pending belief ("recorded; pending your review"); `memory:beliefs` prompt section in the pi-durable section registry (top-15 by confidence + top-5 retrieved vs current user message; embedding-endpoint-down degrades to confidence-only, never fails the turn; omitted when empty). pgvector NOT installed on the scratch PG at test time → 3 DB-backed tests SKIP cleanly (contract documented; re-run `cargo test -p forge-api --test memory_tests` after `sudo pacman -S pgvector`); degradation test ran its 501 branch; all 22 migrations verified to apply in order. Gates: cargo test workspace green, clippy 0, fmt clean, vitest 30/30 (was 26 + 4 memory), verified on disk). H4.2 ✅ (forge `4a2127e8` — `memory_capture.rs` (867 L): TurnEnd handler next to refresh_session_summary (harness.rs; terminal-entry guard `stopReason != toolUse` defers segments); slice = last `pi.user` entry ≤ terminal assistant entry → all `durable_entries` in range (durable schema from state); deterministic extractor (bash commands ≤200 chars ×10, write/edit paths, explicit feedback sentence patterns, implicit re-sent flag), summary = deterministic one-liner (cheap-model pass = documented follow-up), redact() (sk_*/Bearer/password=/token= shapes, unit-tested), embed → `memory::insert_episode` with source.seq_range provenance; exactly-once via overlapping seq_range check; fire-and-forget, errors warn+skip. Pure-Rust extractor + redaction tests + DB test (skip-when-no-pgvector; SQL pre-validated against throwaway scratch PG via psql). Gates: 22 cargo suites green, clippy 0, fmt clean; harness untouched, verified on disk). H4.4 part 1 ✅ (forge `dd737acf` + ranch-2 `9bcb5c3` — governance surface, the mule reflection workflow itself is still ☐ below. `023_belief_rationale.sql` (guarded ADD COLUMN IF NOT EXISTS on `beliefs` — the table stores the proposer's rationale; no existing column fit, decision documented): `Belief.rationale` rides every GET /memory/beliefs response + the card payload. `POST /agents/:id/memory/beliefs/proposals` {actor?, beliefs:[{content, kind?, confidence?, rationale, source_episodes, watch?}]}: per-item near-dup gate against pending/active beliefs — normalized-string equality PRIMARY (case/whitespace-insensitive), cosine > 0.92 secondary when the embedding endpoint answered (degrades to string-only when down, documented) + batch-local dedupe; accept → `upsert_proposal` (pending, v1, audit change=`proposed` with the FULL proposal as detail; actor = body.actor or "reflection") + card push. Cards: agent's most-active conversation (the `GET /agents/:id/active` rule, `last_active DESC`) → `RanchToolQueue.insert_meta` kind `memory_review` (queue entries now carry kind+payload; `resolve` returns them and the `POST /ranch-tools/:id/result` handler applies the transition ON TOP of the oneshot) → `ranch_tool_request` SSE payload `{kind:"memory", agent, belief_id, belief_kind, content, rationale, source_episodes}`. NO active conversation → warn log, belief stays pending, visible via ?status=pending, no re-push in v1 (documented). Answers: keep → active / forget → forgotten / edit → new content + active + re-embed (failed re-embed nulls the vector, not stale) + version bump + audit actor `user-card`; failed/timeout relay = no transition. Direct doors: `POST …/beliefs/:bid/keep|forget` (actor `user:<id>`, 404 unknown). `memory_remember` intentionally gets NO card (user-instructed). ranch-2 lane (`forge.rs`, mirrors the H3.5 policy_ask lane): tool `memory_review` → AgentAsk "I've learned: **{content}** ({rationale}) — keep this memory?" choices [Keep, Forget] + `free_text:true` (EDIT = the free text, precedence over any choice; blank text + choice 0/1 = keep/forget; nothing = failed relay) → ~2 s poll to ~55 s (under the 60 s queue TTL; slow human ⇒ belief stays pending, no re-push) → POST /ranch-tools/{id}/result {output:{action,content?}}; pure `memory_review_reply` mapping unit-tested. Gates: forge workspace test/clippy/fmt green (lib 149; memory_tests 8 incl. the 3 new H4.4 DB tests — those SKIP on this machine: pgvector is still absent from the scratch PG cluster (built pgvector 0.8.7 against PG18, but installing it needs root for the system SHAREDIR; re-run `cargo test -p forge-api --test memory_tests` on a pgvector host to exercise them); pure-Rust halves — string/cosine dup gate, normalize, card-answer mapping on both sides — all run and green); ranch `cargo test -p ranch -q` 46 (45 + 1), clippy 0, fmt; harness untouched, no vitest). Mule reflection workflow + wakes still ☐). H4.4 part 2 ✅ (mule `c217760` — `examples/reflection/` (21 files +2857): 4 wasm modules (pull/check/submit/stub) via the examples/wasm toolchain (GOOS=wasip1), each run through the real wazero engine in tests; step 2 = plain mule AGENT step (stateless cheap-model inference — forge_conversation would park the reasoning in the agent's own transcript; documented); deterministic gate: dup (normalized-string), contradiction (negation-marker set in exactly one pair member + Jaccard token overlap ≥ 0.4, stopword-filtered; both-negative pairs agree in polarity → not flagged; semantic opposition documented out-of-reach for v1), decay `conf*exp(-λ·age_days)` λ=0.02 default; needs_review diffs still submitted with a review prefix (become pending beliefs); creds = workflow input fields (secrets-table upgrade documented); seed.sh + README tracing the full loop. Gates: mule 19 pkgs green BOTH auth modes, build/vet/fmt clean, 4 .wasm in dist/, verified on disk). H4.5 ✅ (forge `619918e8` — `024_memory_trigger_queue.sql` (queue table UNCONDITIONAL — no vector col; belief_id FK + `beliefs.last_triggered_at` ride the guarded-DO pattern of 023; all 24 migrations verified applying in order); watch scan `memory.rs::scan_watch_triggers` in the capture task post-insert (v1: watch.match case-insensitive substring vs summary+feedback, cooldown_hours default 24, queue insert + last_triggered_at in ONE txn; pure helpers unit-tested; errors warn+skip); API `GET /agents/:id/memory/triggers/pending` + `POST …/triggers/:tid/consumed` (idempotent re-ACK, owner-gated, 501 when memory absent); full DB test (watch hit → row + cooldown → no second row + consume flow) skip-when-no-vector. mule `e9f08ec` — `memory_trigger` forwarder lane in internal/wake: env `FORGE_TRIGGER_BASE`/`FORGE_TRIGGER_KEY`/`FORGE_TRIGGER_AGENTS` (empty = disabled, documented), 30 s poll → resolve watch.wake_id locally → in-process `FireWake` → `POST …/consumed` (at-least-once documented; no wake_id → consumed + warn so a misconfigured belief can't block the queue; stale >6 h skipped with warn); tests: fire+ack, no-wake_id, ack-fail retry, stale-skip, disabled. Deviation documented: HTTP pull instead of the plan's shared-DB poll (separate lab containers; same 30 s cadence, more portable). Gates: forge 22 suites green, clippy 0, fmt clean; mule 19 pkgs green BOTH auth modes (one transient 600 s engine timeout under concurrent-suite load — clean in isolation + re-run, verified), build/vet/fmt clean; pgvector still absent → DB tests skip per contract, verified on disk). H4.6 ✅ (forge `bccb7b4c` — `agent_signal(to?, kind, payload)` tool (forge-tools pattern of memory_remember; kind ∈ handoff|insight|request|watch; NULL to = org broadcast; redact + 2560-dim embed → insert_signal); `memory:signals` prompt section beside memory:beliefs (new `GET /agents/:id/memory/signals/unread?limit=10` = render+consume atomically — sender never sees own broadcast; org-broadcast visibility via the EXISTING memory_acl read-grant plumbing, acl_list/org_readable reused — the v1 same-owner simplification was NOT taken; API failure → section omitted, consumes nothing); delegator-scoped episode access: `GET /agents/:id/memory/search?scope=episodes&task_ref=&caller_session=` (task_ref required → 400; caller_session missing → 404 no-leak; parent-chain walk ≤5 hops via sessions.parent_session_id; grant iff a chain session has sessions.agent_id = :id AND sessions.user_id = agents.owner_id — the H2.2 subagent shape; results = task_ref-filtered episodes, cosine-ranked, summaries + source provenance only, nothing transcript-shaped stored to leak). Pure-Rust access-walk tests + 3 DB tests (skip-when-no-vector) + 4 vitest (tool body, section render/empty/failure). Gates: 22 cargo suites (312 passed), clippy 0, fmt clean, vitest 34/34, verified on disk. **PHASE H4 COMPLETE** (H4.1 c9d4ee54, H4.2 4a2127e8, H4.3 w/ H4.1, H4.4 dd737acf+c217760, H4.5 619918e8+e9f08ec, H4.6 bccb7b4c; pgvector install still pending → all memory DB tests run under the skip contract; re-run `cargo test -p forge-api --test memory_tests` after `sudo pacman -S pgvector`.) |
| H5 | ✅ complete | Proactivity: read-only research tasks (H5.1), wake conditions (H5.2), Activity View (H5.3: daemon poller + Meta/activity frames + pause kill switch + TUI + mobile-native + Expo UIs). Digests fold into H6.1's notification policy (plan-sequenced). | forge, mule, ranch | H5.1 ✅ (forge `883ace25` + ranch-2 — proactive research. forge: `agent_research` (migration 025, `pending→running→done→adopted/discarded`, `?open=1` = unresolved) + `POST/GET /agents/:id/research` (owner-gated 404s, restricted keys may use it, 503 when the harness is disabled); the `spawnResearch` harness RPC creates a durable conversation whose per-task extension registry is the read-only surface `{read, webfetch, search, note}` — write-class tools, subagent and memory tools are NOT offered at all (structural enforcement; the `extensionTools` RPC is how the e2e asserts absence against the live registry; boot re-install rebuilds research instances from the `forge.meta` `research:true` flag); `webfetch`/`search` are GET-only with the SSRF guard (private/loopback/link-local refused, no credential forwarding, redirects reported not followed, 30 s + 32 KB); on task settle the `research_report` document lands (newest `pi.assistant`, falling back to `research_notes`) + a `research_report` card on the agent's most-active conversation (same `RanchToolQueue` as H4.4); answers apply via `POST /ranch-tools/:id/result`: Use→`adopted` + `research_resolved` bus/SSE, Discard→`discarded`, Ask-more→ the follow-up is re-injected into the SAME read-only research conversation (user row + bus publish + `submit`) and the row goes back to `running` (failed submit ⇒ row stays `done`). The lossy events-socket race (started/terminal events can land before the session stamp / row insert) is closed by three re-derivations: task_id backfill from the first terminal task at settlement, a post-insert settle re-check in the POST handler, and `resync_unsettled_research` on reconnect (alongside H2.6 `resync_unprojected`). Live e2e (real harness child + faux provider): create → registry absence → report document → card pending → Use → adopted + `research_resolved` + no longer open; child reaped on every exit path (Drop guard). Gates: forge `cargo test --workspace` green (the e2e runs in 2.4 s), clippy 0, fmt clean, harness vitest 50/50 (incl. 13 webfetch guard tests + 3 research tests), zero leaked `main.ts` processes; `ARCHITECTURE.md` §10 documents the section. ranch-2: the `research_report` card lane in `forge.rs` (mirrors `memory_review`): AgentAsk "**{question}** / {summary} — Use this, discard it, or ask for more?" choices [Use, Discard, Ask more] + free text (the follow-up); ~2 s `AgentAskStatus` polls under the ~55 s deadline (just under forge's 60 s `RANCH_TOOL_TIMEOUT`, so a slow human surfaces as a failed relay and the row stays `done`/open); pure `research_report_reply` mapping unit-tested (Use/Discard/Ask-more + bare-text-means-follow-up + blank-ask-more-is-no-answer). Same commit fixes the card-lane relay: the answer is now posted as the result body's TOP-LEVEL `output` (`relay_card_result_to_forge` flattens the executor's inner `output`) — the old `relay_result_to_forge` posted the whole reply, nesting `action` one level too deep so forge's apply handlers saw no answer (latent since H4.4's `memory_review` lane, which the card now also uses). Gates: `cargo test -p ranch -q` 47 (46 + the mapping test), clippy 0, fmt clean; design/agent-tools.md documents the card bridge.) | H5.1 ✅ (forge `883ace25` — `025_agent_research.sql` (agent_research lifecycle table: pending|running|done|adopted|discarded + resolution); `POST /agents/:id/research {question, scope?}` → `spawn_research` harness RPC creates a `research`-kind durable task whose ToolTask registry is FILTERED PER-TASK to {read, search, webfetch, note} — bash/write/edit/web-post/email/ranch_*/mule are literally absent (structural enforcement: the model never sees a write tool); `webfetch` = new GET-only in-process tool (no auth/cookie headers ever, http/https only, private-IP/localhost refused — SSRF guard, ~32 KB truncation); `note` appends the task's private research_notes document; completion: task terminal → research_report document + `research_report` suggestion card through the RanchToolQueue/ranch_tool_request machinery (Use/Discard/Ask-more; Ask-more = free_text follow-up); state transitions + `research_resolved` bus event; `GET /agents/:id/research?open=` for H5.3. Two real bugs found+fixed by the e2e: (1) test UUID-bind bug (silent uuid=text operator error swallowed by .ok().flatten() — the state poll never saw a row), (2) lifecycle race — spawnResearch admits the prompt inside the RPC so task_state events can precede the durable_conversation_id stamp + agent_research row: fixed with task_id backfill from the first terminal task at settlement, post-insert settle re-check, and resync_unsettled_research on every events-socket reconnect (alongside H2.6's resync_unprojected). e2e now 2.4 s with a ChildReaper Drop guard; ipc.test.ts also fixed (detached spawn + process-group SIGKILL — npx's tsx child was orphaning). ranch-2 `d8d5e83` — research_report card lane in forge.rs (mirror of memory_review: Use/Discard/Ask-more + free_text, 2 s poll to ~55 s, pure mapping fn unit-tested; 47 tests). Gates: forge 23 test targets green, clippy 0, fmt clean, vitest 50/50 (was 34 + webfetch/research), zero orphaned harness processes, verified on disk). H5.2 ✅ (forge `bbdde8e1` (supersedes the same-content 12cc4a11; both unpushed, object retained) — the three remaining wake-matrix rows, NO mule changes needed (sinks pre-existed: H3.3 mule `e09ddc0` fire ingress/event lane, H4.5 mule `e9f08ec` forwarder): (1) file-watch: `filewatch.rs` inotify worker, config `FORGE_FILEWATCH="path:agent,path:agent"` (UUID or agent name per path; last-colon split; worker starts only if ≥1 watch survives boot validation; Linux-only; debounced/coalesced) → additive `BusEvent::FileChanged` + lands in the agent's conversation via the H2.3 timer sink (no active session → warn+drop); (2) turn_ended → mule event-wake forwarder (`wake.rs`): config `FORGE_TURNEND_MULE_BASE`+`_KEY` (no key → not started), session→agent resolution, fire-and-forget `POST /api/v1/wakes/fire {kind:"event", event_tag:"agent.turn_ended"}` (mule's event lane matches spec.type against event_tag — contract documented), mule-down/5xx = warn-only; (3) `schedule_reminder` tool (all agents): server endpoint `create_session_reminder` → durable H2.3 timer on the calling session; `agent_signal` push-wake optional, off by default (same knob pattern); `scripts/herd-wake-scenario.sh` (312 L, manual fleet acceptance: the plan's three-hop scenario webhook→agent turn→turn-end triage wake→episode→memory-trigger wake, per-hop PASS/FAIL + wake_log evidence, env-gated, `bash -n` clean — NOT run in CI). 9 tests in herd_h52_tests.rs (forwarder body + 500-warn, file-watch bus event, reminder creation/4xx, signal-push) + harness reminder.test.ts (3). Gates: 24 cargo test binaries green (incl. 9/9 h52 in 3.6 s), clippy 0, fmt clean (subagent had to run fmt the previous agent skipped), vitest 53/53, zero orphaned harness procs, verified on disk). H5.3 part 1 ✅ (forge `96c31aef` — `026_agents_paused` + `POST /agents/:id/pause`/`resume` (idempotent, owner-gated); admission enforcement: dispatch_message + agent-talk → 409 'agent paused' BEFORE any row lands; timer-fire seam = the HARNESS (timers.ts fire() checks agents.paused via sessions.durable_conversation_id after the exactly-once claim, before submit — paused = logged no-op; lost-tick: a paused one-shot timer's tick is lost by design, recurring re-arms; in-flight turns finish; reads stay open — pause halts new work, not observability); `GET /agents/:id/tasks` made REAL (was H1.1 stub): durable_tasks.conversation_id IN (sessions where agent_id + durable_conversation_id set), ?state= closed-set filter default non-terminal, ?limit= 100/500, parent_task_id from record.owner; no wall-clock ts on durable_tasks — recency ≈ task id (documented). mule `36d0b9e` — `POST /api/v1/wakes/pause {agent_id, paused}` bulk enable/disable (owner-scoped, target.agent_id match, {affected:N}) + `GET /api/v1/wakes/activity?since=&limit=` recent wake_log feed. Gates: forge 25 test binaries, clippy 0 (fixed a pre-existing H5.2 precedence warning), fmt clean, harness 55 tests; mule 19 pkgs BOTH auth modes, build/vet/fmt clean; zero orphans, verified on disk). H5.3 part 2 ✅ (ranch-2 — the Activity View daemon side: `activity.rs` poller worker (AGENTS.md shape: long-lived thread + OWN pipe pair; the daemon registers the pipe reader as the synthetic "activity" client and its `ActivityPush` frames broadcast to every client like any client frame; stop flag + join on daemon shutdown — HTTP calls are 10 s-capped so the join is bounded); 5 s poll: local items from a shared `LocalState` the poll loop refreshes every tick (pending asks — never pruned, the user may answer hours later; agent-spawned panes incl. awaiting-approval; recent trigger fires — new bounded ring `triggers::Scheduler.recent_fires`, cap 32, 10-min window; machine-launched pane-less mule jobs, 10-min window) + per-agent forge feeds (`GET /agents` + `/tasks?state=pending,running` + `/memory/beliefs?status=pending` + `/research?open=1`, 10 s timeouts, a failed feed skipped silently, down forge/mule = one-time warn + degraded feed, never a stall) + mule `GET /api/v1/wakes/activity` (RFC3339 since-watermark — advances on success only; 10-min window). DIFF + DEBOUNCE: item key (kind, agent, id), full-item equality; on change vs last broadcast → wait 2 s → re-poll → broadcast the full set only if still changed (churn that settles inside the window never ships; a failed pipe write leaves last_broadcast untouched so the next cycle retries). 8 item kinds: ask/spawn/trigger/mule_job (local, agent=None — local panes carry no agent identity) + forge_task/belief_pending/research_open/wake (agent = registry name; durable tasks carry no ts → fetch time, documented). PUSH = dedicated `ActivityPush { items }` frame, NOT `Meta { kind:"activity" }` — `Meta` has only a `status` string, no payload slot for structured items; the decoder drops unknown tags so older clients are unaffected (additive-ness tested). PAUSE: `AgentPause { req_id, agent?, paused }` → per target (id or name, case-insensitive; no target = all known agents — registry kept warm by `AgentListOk` + the poller's own `GET /agents`, fresh-daemon fallback = one live list fetch; zero targets → Error frame): forge `POST /agents/:id/pause|resume` (idempotent) + mule `POST /api/v1/wakes/pause` best-effort (no mule config ⇒ leg skipped, `mule=None`, still ok; per-leg failures in `AgentPauseResult.error`, both legs still attempted) → `AgentPauseOk { req_id, results: [{agent, name, forge?, mule? (="updated"), error?}] }`. Documented semantics: pending asks STAY VISIBLE while paused (pause halts new work, not observability — forge reads stay open for paused agents). CLI: `ranch pause|unpause [AGENT]` (no arg = all) — "unpause" because `ranch resume` is the existing forge-session resume command. Pure logic unit-tested: set diff (key + content, order-insensitive), 200-char char-safe truncation, newest-first stable sort, parse fns for every feed (forge tasks/beliefs/research, mule activity + pause — incl. mule's actual `updated` field), mule pause body exact JSON + RFC3339 since query, fire-ring bound/eviction. PROTOCOL.md §8.6 documents the family. Gates: `cargo test -p ranch -q` 59 (47 baseline + 12), clippy 0, fmt clean; `cargo test -p ranch-protocol -q` green. Live forge/mule NOT exercised here (HTTP goes through the existing `http_json` seams + a timeout-capped ureq agent — same localhost pattern as AgentTalk; verified against the live endpoint SHAPES in forge `96c31aef` / mule `36d0b9e` source, read-only).) H5.3 part 3a ✅ (ranch-2 `7b86899` — TUI Activity View in client.rs: dashboard now sessions → agents → activity (Tab-cycle; parallel to the H1.3 sections); ActivityPush arm replaces the cached item set + clamps the cursor (push-only, TUI never polls); timeline groups ordered by most-recent item, machine-local items under a `local` header, empty state; glyph map (🧠 belief, ✅ ask, 🔬 research, ⏰ wake, ▸ forge_task, ⇄ mule_job, ⚡ trigger, ✚ spawn, · unknown); keys in the Activity section: Enter/o deep-link (H1.3 AgentTalk path; ask items flash-if-pending else open conversation), j/k move, `p` = pause/resume HERD (AgentPause{None}), `P` = pause/resume the highlighted item's agent (machine-local items flash 'no agent'; no shadowing — `p` still means new-pi-session in the other two sections); AgentPauseOk results flash the status bar + a visible paused header state. 10 new unit tests (grouping/sort, pause payload, cache update/clamp); 69 tests, clippy 0, fmt clean, verified on disk. Rendering NOT visually verified headlessly — subagent said so; eyeball on first attach. Stop/abort action deferred (v1 = [open] only — documented follow-up). H5.3 part 3b ✅ (ranch-2 `ad8ce74` — mobile Activity UIs, both phone clients, rendering the same `ActivityPush` frames the TUI renders. mobile-native `HerdActivity.kt` (new; "Activity" in Settings & tools' Tools row next to Agents; manifest + README updated): push-only timeline — `ActivityPush` replaces the in-memory item set (no polling, no persistence in v1); items grouped by agent (machine-wide ask/spawn/trigger/mule_job items under "LOCAL"), groups ordered by most-recent item, rows "glyph text · HH:MM" with the TUI's H5.3 glyph map (🧠/✅/🔬/⏰/▸/⇄/⚡/✚/·); header "Pause herd"/"Resume herd" sends `AgentPause { agent: null, paused: toggle }`, long-press on an item offers "Pause/Resume agent <name>" — both toggle off the last CONFIRMED target (`AgentPauseOk` all-ok sets/clears the hint; a failed leg leaves it unknown) and toast the per-agent results (the TUI's `activity_pause_status` shape: N ok / N ok M failed + first failure detail); tap deep-links via the H1.4 `AgentTalk{continue}` path (name → id from a cached `AgentList`; machine-local items flash "open it from the sessions list" — v1 has no cross-pane focus). `Term.kt`: `agentPause()` + `ActivityItem`/`AgentPauseResult` parsers matching `ranch_protocol` exactly (optional `agent`/`forge`/`mule`/`error` via has()+isNull()). Expo `mobile/screens/Activity.tsx` (new "activity" tab between agents and automation — decided: separate screen, not a section of Agents.tsx, which is registry + profiles): same timeline/pause/deep-link semantics (Alert dialogs for agent-pause, same hint rule, `AgentPauseOk` status line); `lib/frames.ts` gains `ActivityItem`/`AgentPauseResult` + the `ActivityPush`/`AgentPause`/`AgentPauseOk` variants; the tab reuses the agents tab's `onTalk` path so App's main `AgentTalkOk` handler attaches the pane. Gates: mobile-native `./gradlew assembleRelease` BUILD SUCCESSFUL (JDK 17, AGP 8.12.0; APK 1.3 MB) + `compileReleaseKotlin` clean; mobile `npx tsc --noEmit` clean (no build script in package.json). Device rendering NOT verified headlessly — eyeball on first attach; digests still → H6.1) |
| H6 | Presence & voice: mobile agent notifications+actions, voice input, voice loop v1 + barge-in v2, specialist provisioning UX | ranch (TUI/web/native), mule, forge | H6.1 ✅ (ranch-2 `9f734ba` — agent-scoped notifications in Notify.kt: kinds turn_end (Meta agent working→idle edge) / errors (⚠ rows) / questions (plain AgentAskRequest) / approval (choices Allow this action|Deny — the H3.5 lane fingerprint) / research (Use|Discard|Ask more) / memory (Keep|Forget) / task_done (ActivityPush forge_task leaving the set; Open via AgentTalk{continue} + Fork via AgentTalk{mode:fork}) / every_message (off); lanes fingerprinted by choice sets (AgentAskRequest carries no lane field — verified against forge.rs's three card lanes); avatar in BigPictureStyle (custom small icons not allowed on modern Android); buttons work locked/backgrounded (PendingIntents → MonitorService.onStartCommand → RelaySession.send, queues until channel join); per-agent mute/priority in Prefs.kt + Notifications screen (mute = notification-only, Activity feed unaffected); Expo parity via `mobile/lib/notify-policy.ts` (policy constants as source of truth, Kotlin mirror commented to it). TaskAbort (task 'stop') deferred — matches H5.3 v1. Gates: APK BUILD SUCCESSFUL (1.3 MB), tsc clean, verified on disk). H6.2 ✅ (ranch-2 `feb68e9` — voice input: mobile-native `Voice.kt` (new) — two modes under Prefs `voice_input`, DEFAULT `forge-stt` (the H6.3 voice-loop path): (a) forge-stt push-to-talk — ACTION_DOWN → `MediaRecorder` 16 kHz mono m4a/AAC memo, ACTION_UP → OkHttp multipart `POST {forge}/v1/audio/transcriptions` (`file` + `model=parakeet` + `response_format=json`, `X-API-Key`, 30 s timeout — the exact shape forge's own web UI sends, verified against `api/voice.rs` + `web/app.js`) → Parakeet → transcript appended to the composer (user still hits send; v1, documented); the phone talks to forge DIRECTLY on this path (no forge channel through the daemon), so Settings gains forge base URL + API key fields (asymmetry documented: recognizer needs no config). (b) recognizer — Android `SpeechRecognizer` (`RecognizerIntent`, `EXTRA_PREFER_OFFLINE`): the airplane-mode path, no new deps. Mic button 🎤 next to send in `SessionActivity`'s chat row; red ⏹ state + status line while recording (level meter deliberately out of scope); misconfig/5xx (503 "speech-to-text is disabled" / 502 "Parakeet STT backend unreachable") flash the status, never crash (M8.4). `RECORD_AUDIO` runtime-requested at first forge-stt use; denied → toast + status flash + recognizer fallback for that tap. `SettingsActivity` gains a Voice input section (mode toggle, forge base/key, mic-permission button); manifest RECORD_AUDIO; README §voice input. Expo parity (web only — the on-device recognizer has no web equivalent, documented): `lib/voice.ts` (config load/save + `transcribeViaForge`, same multipart shape + X-API-Key) + hold-to-talk 🎤 in `Terminal.tsx`'s chat input (browser `MediaRecorder`, webm/opus preferred, same as forge web) + forge base/key fields in `Settings.tsx`. Gates: `assembleRelease` BUILD SUCCESSFUL, `tsc --noEmit` rc=0, verified on disk; MIC NOT device-verified (no phone session) — the plan's one-real-sample acceptance is still owed.). H6.3 ✅ (mule `8b402f0` + ranch-2 `f32a00f` — voice loop v1: mule-orchestrated push-to-talk "call", host/phone ⇄ mule direct WebSockets (no daemon in the media path, ZERO new ranch frames — PROTOCOL.md §8.7 documents the decision; §10.1's `VoiceCall` row marked not-shipped). **Mule**: new `internal/voice/` package — Call session + Hub, a single-loop state machine (listening → utterance → thinking → speaking) in a goroutine tree; energy-based VAD (600 ms boundary, no dep) + barge-in v1 (3-frame RMS hysteresis while TTS plays → playback stops, the utterance wins); `voice_call` engine step type (validator + `VoiceExecutor` seam, job stays RUNNING until hang-up, crash → job failed = "call dropped"); unauthenticated `/voice/{job}/in|out` routes (the 128-bit job id IS the capability — same posture as /ws); in socket: binary 16 kHz mono s16le ~30 ms frames, text `{"hangup":true}` / `{"text":…}` (typed-utterance fallback, no STT hop), socket close = hang-up; out socket: binary TTS PCM at the step's `tts_sample_rate` (default 24 kHz, out socket carries no rate metadata — surfaces read the step config), text `{"assistant":…}` per turn + `{"done":true}`. Agent steer: `POST /messages` into the agent's active conversation with mode "steer" (forge has no separate mid-turn delivery API — H2.1's `delivery:"steer"` is the forge-task concept; utterances are tagged `[voice call] …`; the mode stays in the step config so a real mid-turn API is a config-only upgrade later). `examples/voice-loop/` (voice-loop.json + idempotent seed.sh + README: data-path diagram, transport decision, barge-in, kill semantics). spec.md "Voice loop (H6.3)" section. **Ranch CLI** `ranch call [agent]` (`voicecall.rs`): registry resolve → workflow find (matching voice_call `config.agent_id`) → job start → two tungstenite sockets; arecord/parecord 30 ms frames else typed-text fallback (TUI parity, not fake); aplay/paplay @ TTS rate else printed assistant frames; bye/Ctrl-C/mic-EOF = hang-up. **Mobile** 📞 on each agent row → `CallActivity.kt`: AudioRecord → in socket; out socket → AudioTrack; assistant transcript; typed-text row on mic denial; mule base+key in Settings (next to the H6.2 forge fields). Also: `one_shot()` (client.rs) no longer spins forever when a stale daemon drops a new frame family — 30 s deadline + "daemon may be out of date" error. Gates: mule `go build`+`go vet`+gofmt clean, `go test -count=1 ./...` 20 packages ok (also under `MULE_NO_AUTH=1`); ranch-2 74 tests (69 baseline + 5 new: in-frame mapping, ws-URL conversion, TTS-rate bounds, mic command shape), clippy 0, fmt; mobile `assembleRelease` BUILD SUCCESSFUL. **Owed**: the 5-min / 20+ turn / barge-in acceptance and the real mic→Parakeet→Kokoro path — no real audio/Parakeet/Kokoro on this box; the full call loop + barge-in + kill semantics are covered by mule unit tests against a fake forge, not by a live call.) H6.4 ✅ (mule `e8f4c86` + ranch-2 `d4a07df` — full-duplex barge-in v2. **Mule voice-loop v2** (`internal/voice`): in-frames now processed in real time around every blocking hop — `pollHop` pumps inCh/textCh while the STT/agent/TTS hop runs in a sub-goroutine (v1 only drained the in-queue between out chunks); the in-utterance buffer is continuous across the thinking→speaking state change on a barge-in, so there is no VAD re-arming gap when TTS starts. Ratio endpointing replaces the fixed 600 ms boundary: an utterance ends at trailing silence ≥ `silence_ms` (default now 500) AND speech:silence RMS ratio ≥ `endpoint_ratio` (3.0); digital silence passes as an infinite ratio; `max_utterance_ms` (15 s) force-ends runaway recording; `DecideEndpoint` is a pure function with synthetic-PCM unit tests. In-turn steer: overlap speech during PhaseThinking is endpointed and submitted fire-and-forget — early partial at the first ~silence_ms pause (tagged), full utterance at endpoint; no TTS answer (the in-flight turn owns the speaker; forge has no partial-prompt/cancel surface). Fast barge-in: on trigger, `flushOut` DRAINS the out-queue then emits the `"flush":true` control on the out socket; stop-to-flush asserted <150 ms, 10/10 consecutive interruptions. `emitOut` never blocks: a slow client triggers drop-oldest on the bounded out-queue (`out_dropped` step output) so out pressure can never pause in-VAD bookkeeping (`TestCallDualChannelLiveness`). New config: `silence_ms` 500, `endpoint_ratio` 3.0, `max_utterance_ms` 15000. **Mobile**: `CallActivity` echo guard (per-frame RMS hysteresis → `AudioTrack.setVolume(0f)` while the mic is hot; 400 hot / 200 cold, ~90 ms mute / ~240 ms unmute — sits just under mule's 600 barge-in floor so echo can't reach barge-in) + `"flush":true` handling (`stop→flush→play`). **Spike**: `.spike/voice-latency/` — `measure.sh` (STT/TTS/TTT budgets, curl+python3, validated against mocks; REAL numbers pending — no Parakeet/Kokoro on this box, no fake numbers recorded) + `RESULTS.md` template + README. **Docs**: spec.md “Voice loop v2 (H6.4)” (honest v1→v2 delta, flush-control protocol, echo guard, STT-partial status, v2 limits) + examples/voice-loop README. **Test fixes**: fakeforge now tags TTS calls at request ARRIVAL — H6.4 overlaps in-flight TTS calls (barge-in abandons one while the next is in flight) and completion order is not request order under load, so the old tag assignment silently crossed responses between concurrent handlers; the flaky barge-in test now holds the TTS hop open 100 ms (zero-delay hops made the speaking window a scheduling coin-flip under real-time frame pumping) and asserts the next-audio-after-flush invariant instead of a fixed fake tag (the abandoned call's POST can land late). Gates: mule `go build`/`go vet`/gofmt clean, `go test -count=1 ./...` + `MULE_NO_AUTH=1` green (20 packages), the previously-flaky barge-in test 180/180 `-count=30` under both load profiles; mobile `assembleRelease` BUILD SUCCESSFUL. **Owed**: real STT/TTS/TTT numbers (spike run), on-device mic echo behavior, 10× mid-sentence interrupt acceptance on real hardware — all covered by mule unit tests against a fake forge, not by a live call.) H6.5 ✅ (forge `3a943d6f` + ranch-2 `aa24619` — provisioning wizard + web_login + credentials scope: forge secret-store seam (migration 027 `secrets` named-slab + `agents.org_id`; `/secrets` API lists names+expiry only, values never serialize) + `resolve_credential_env` (credentials_scope.env_refs → live values at exec time, present/absent/expired fail-closed, slot names only in errors; both bash paths ride the container env via `ContainerEnv.extra_env`/`--setenv` — single source of truth `nspawn_args`, unit-tested) + web_login broker (`POST /sessions/:id/web-login` → RanchToolQueue kind `web_login` card `{url, slot}` ONLY → one-time handoff token 30 min single-use → xdg-open `/auth/browser-handoff` in the user's own browser → cookie pasted by the USER → secrets table; `web_login_done` = `GET …?slot=`; harness `web_login`/`web_login_done` in-process tools, status-only outputs, CLIENT.md; GREP AUDIT: cookie absent from every messages row + card payload + response — herd_h65_tests.rs 5 + harness web-login.test.ts 8). ranch-2: `ranch agents new` wizard (`agents_new.rs`: profile picker, tools allowlist, memory scope, home machine, "07:00"→"0 7 * * *" cron builder) + mobile ＋Agent form (`NewAgentActivity.kt` + `AgentProvision`/`AgentProvisionOk` frames — daemon runs the SAME `provision()` core: POST /agents + best-effort mule cron wake `target={forge_conversation:{forge_url,api_key,agent_id}}`; empty home_machine = the daemon's own relay machine id) + the daemon's `web_login` card lane (AgentAsk "Sign me in"/"Cancel" → `agent_ask_round_trip` helper refactored out of the H3.5 policy lane → `/ranch-tools/:id/result`). Docs: agent-builder.md §7 (flow diagram, invariants, v1 limits), PROTOCOL.md (web_login card kind + AgentProvision family), AGENTS.md CLI line, mobile-native README, build log. Gates: forge 24 test targets + clippy 0 + fmt + harness 63/63; ranch-2 104 tests + clippy 0 + fmt; mobile assembleRelease BUILD SUCCESSFUL. **Owed**: the 24h proactive-turn acceptance (needs live forge+mule); Expo/web form parity; credential slots stay out of the provisioning request by design (declare via PATCH /agents/:id + the web_login card).) |

---

## 3. Phase H0 — Foundations & the big decision

**Goal: leave this phase with (a) a working Postgres storage backend for
pi-durable that passes its conformance suite, (b) the runtime decision
recorded (pi-durable used directly — see H0.3), (c) pgvector in
production, (d) all pi/pins frozen.**

### H0.1 — Clone & digest pi-durable (forge repo, docs-only)

- Clone `github.com/earendil-works/pi` (sparse: `packages/durable` only)
  into `~/src/forge/vendor/pi-durable/` (gitignored, like
  ranch-2's `vendor/ghostty`). Pin the commit in
  `vendor/pi-durable/.pin` (commit SHA + `@earendil-works/pi-durable`
  version `1.0.4`).
- Read in order and leave a 2-page notes file at
  `~/src/forge/docs/HERD-PI-DURABLE-NOTES.md`: `packages/durable/
  docs/spec.md`, `src/harness/`, `src/session/` (transaction, forks,
  observation), `src/tasks.ts`, `src/documents.ts`, `src/types.ts`,
  `src/testing/storage-conformance.ts`. The notes must record, for
  each harness concept, the exact TypeScript type/field names — the
  Rust client (H2.0) and the `durable-pg` schema (H0.2) mirror them
  verbatim.
- **Acceptance**: notes file exists; a human can read it and map every
  §1.4 record type to a `durable_*` table column.

### H0.2 — Postgres storage backend + conformance run

**DECIDED (user, 2026-10-06): use pi-durable as the runtime —
`github.com/earendil-works/pi` `packages/durable` — do NOT reimplement
it.** This task therefore builds the one thing pi-durable needs from
us: a Postgres `Storage` backend, in TypeScript, validated by
pi-durable's own conformance suite. This backend is a permanent,
load-bearing artifact (forge's durable core runs on it from H2 on),
not a throwaway spike.

- New package `~/src/forge/durable-pg/` (in-repo, committed; TS,
  vitest, `pg` driver; eventually published to our registry as
  `@forge/durable-pg`):
  - `src/storage-pg.ts`: implements pi-durable's `Storage` interface
    (`dist/types.d.ts:787`) over Postgres. Table layout:
    `durable_conversations`, `durable_entries`, `durable_tasks`,
    `durable_submissions` (UNIQUE(conversation_id, request_id) for
    `submissionByRequest`), `durable_documents` (incarnations +
    addresses + points), one monotonic `durable_seq` allocator
    (single-row `UPDATE … RETURNING seq+1`, cf. forge's
    `004_get_next_sequence_locking.sql`). `commit` = one Postgres
    transaction. Postgres gives us multi-writer safety that the
    bundled sqlite backend can't: forge-api (Rust) and the
    harness (Node) both read/write the same DB directly, which is
    the whole point of this backend.
  - `test/conformance.test.ts`: run `packages/durable/src/testing/
    storage-conformance.ts` against `storage-pg.ts`.
  - `test/kill9.test.ts`: start a harness conversation on this
    backend, kill -9 the node process mid-turn, `harness.resume()`,
    assert the conversation reaches a consistent completed state
    with no duplicate submissions.
- Run against a scratch DB **inside the forge machine** (a second
  database `durable_spike` — zero prod impact) or a local `pg`
  container on this box if the machine is busy rebuilding (H0.5):
  `createdb durable_spike`.
- **Acceptance**: full conformance suite green; kill-9 test passes
  twice in a row; a `durable-pg/README.md` with: timing numbers vs
  the sqlite backend (`storage-benchmark.ts`), and any semantics
  that were expensive/awkward in Postgres (partial updates? row
  versioning? the `head` marker walk?).

### H0.3 — Decision record (closed)

- Write `~/src/forge/docs/HERD-DURABLE-DECISION.md` recording the
  decision: **Node runtime running pi-durable directly, with the
  `durable-pg` Postgres backend (H0.2)**. Rust does not reimplement
  the harness. Document the consequences and boundaries (they are
  binding for H2):
  - **Boundary**: the Node harness process is the only writer of the
    `durable_*` tables; forge-api (Rust) reads them directly for
    queries/SSE but effects (new turns, tool results, aborts) go
    through the harness via its IPC. Tenancy, agents, memory,
    profiles stay in Rust-owned tables.
  - **Versioning**: pi-durable is pinned (H0.1) and the
    `durable_*` schema is owned by `durable-pg` migrations; schema
    changes ride the same deploy as the image rebuild (H0.4-style
    reconcile), never an ad-hoc drift.
  - **Kill criteria** (what would reopen this decision): the
    conformance suite cannot pass on Postgres; per-turn IPC overhead
    makes voice-loop latency targets (H6.4) unreachable; or
    upstream pi-durable churn breaks us faster than we can pin.
- No further user sign-off needed (§12 Q1 is answered); the decision
  doc is the record.

### H0.4 — Infra: pgvector on the forge machine; new fleet `mule` machine

Both tasks are **`~/src/lab` machine-config work + rebuild/reconcile on
mini** (follow `lab/ct/forge/README.md` for the image-build path):

- **pgvector on the `forge` machine** (Postgres already lives inside
  it — `lab/ct/forge/configuration.nix:44–47`, `postgresql_16`, db
  `forge`): add pgvector to the Postgres package
  (`postgresql_16.package =
  pkgs.postgresql_16.withPackages (ps: [ pkgs.pgvector ])` style, or
  `environment.systemPackages`) and make the existing `forge-db-init`
  service (configuration.nix L114–119) run `CREATE EXTENSION IF NOT
  EXISTS vector;` on the `forge` db — the service already runs
  post-init SQL (L95–102), so this is a few lines. Rebuild the image
  + `sudo lab-alc-reconcile`; verify from the VM:
  `psql -U postgres -d forge -c 'select extversion from pg_extension
  where extname='''vector''''`.
- **New private `mule` machine** (`lab/ct/mule/`, modeled on
  `lab/ct/mule-public/` but private/fleet): NixOS config with
  PostgreSQL inside (db `mule`), mule service with the binary baked
  into the image (`lab/ct/mule-public/flake.nix` shows the closure),
  egress rules allowing loopback (mule↔postgres), the `forge` machine
  host (API :8080) and `bifrost` (embeddings); secrets at
  `/Users/jbutler/.lab-alc/data/mule/mule.env` (new Bitwarden vault
  item `mule`) carrying `DATABASE_URL`, `FORGE_URL`, `FORGE_API_KEY`.
  `sudo lab-alc-reconcile` creates it; expose its API on a mini port
  (pick a free one, e.g. `10.10.199.29:8090`, record it here and in
  §0).
- **Repoint consumers**: this box's `daemon.toml` `mule_url`
  (`127.0.0.1:8080` — stale) → the new mule machine's URL; the
  `ranch-mini`/`ranch-demo` machines likewise; then edit §0's
  topology block with the final URLs/ports of every Herd-relevant
  machine.
- **Acceptance**: `vector` extension active on the forge db; the
  `mule` machine boots, `/health` answers, and a test job can reach
  forge over the wire; §0 updated.

### H0.5 — Freeze pins

- **One pi pin for the whole herd.** The forge image bakes pi 0.84.4;
  this box runs 1.0.4. Decide at this task (default: **bump to the
  current stable, 1.0.4**) and record it in §0 +
  `lab/ct/forge/README.md` + `lab/ct/mule/README.md`. If 1.0.4
  changed the RPC surface vs 0.84.4, fix the two consumers *before*
  rebuilding images: forge `pi_agent.rs` (`PiEvent` L97, `PiInput`
  L228) and mule `pirc/pibridge.go` (`RPCCommand` L104) +
  `pirc/event_mapper.go` — run their tests against the new pi.
- `ranch-2`: local pi via `RANCH_PI_BIN`/PATH — have the daemon warn
  at boot when its pi version ≠ the §0 pin.
- `forge`: replace `"@mariozechner/pi-coding-agent": "*"` in
  `extensions/forge-tools/package.json:20` and
  `extensions/ranch-tools/package.json` with the pinned version; the
  image build (`lab/ct/forge/flake.nix`) must install the same pi
  version as the extension pin.
- `mule`: `pirc/pibridge.go` (`buildArgs` L250) — same pin in the
  `lab/ct/mule` image.
- `pi-durable`: pin recorded in H0.1.
- **Acceptance**: `grep -rn '"\*"' ~/src/forge/extensions/*/package.json`
  empty; one pi version string in §0 matches every image + box.

## 4. Phase H1 — The Agent entity & registry ("talk to agent anywhere")

**Goal: `agents` is a first-class forge row; from any ranch surface
(TUI/web/mobile) you can list agents and start/continue/fork a
conversation with one of them. This is the identity layer everything
else (memory, notifications, activity) hangs off.**

### H1.1 — Forge: `agents` table + API

New migration `crates/forge-api/migrations/016_agents.sql`:

```sql
CREATE TABLE agents (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id           UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name               TEXT NOT NULL,
    avatar_url         TEXT,
    home_machine       TEXT,                 -- ranch machine id the agent "lives on" (nullable)
    primary_profile_id UUID REFERENCES profiles(id) ON DELETE SET NULL,
    visibility         TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','org')),
    memory_scope       TEXT NOT NULL DEFAULT 'agent' CHECK (memory_scope IN ('agent','org')),
    tools_allowlist    JSONB NOT NULL DEFAULT '[]',   -- empty = profile's tools
    credentials_scope  JSONB NOT NULL DEFAULT '{}',   -- specialist-dot creds (H6.4)
    extra_instructions TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (owner_id, name)
);
ALTER TABLE sessions ADD COLUMN agent_id UUID REFERENCES agents(id) ON DELETE SET NULL;
CREATE INDEX idx_sessions_agent_id ON sessions(agent_id);
```

- New module `src/api/agents.rs` (mirror `api/profiles.rs`, 481 lines,
  as the CRUD template): `POST/GET /agents`, `GET/PATCH/DELETE
  /agents/:id`; tenancy via the same `session_access_err`-style
  owner-or-admin gate (`api/messages.rs:26` pattern; `can_access` in
  `api/auth.rs`).
- Conversation endpoints on agents (these are what ranch H1.3 calls):
  - `GET /agents/:id/conversations?limit=&latest=1` → sessions
    joined on `agent_id`, most-active first.
  - `POST /agents/:id/conversations` `{title?, fork_from?:
    session_uuid, cwd?}` → creates a session bound to the agent
    (profile = agent's `primary_profile_id`), optionally
    `fork_from` a session of the same agent (fork semantics: new
    session rows copy the transcript up to the fork point until H2
    lands native forking — pre-H2, implement as "copy messages with
    sequence reset"; post-H2, this becomes pi-durable's conversation
    fork and this endpoint keeps its shape).
  - `GET /agents/:id/tasks` (stub returning `[]` until H2.3; keep the
    route — H5's Activity View depends on it existing).
- Routes added in `create_router` (`api/mod.rs:923`): the block at
  L939–945 (profiles) is the template.
- **Acceptance**: `curl` round-trips against the live forge machine
  (`http://10.10.199.29:8110`); a session created via
  the agent endpoint shows in `GET /sessions` with `agent_id` set;
  restricted API keys (`015`) cannot create agents (enforce in
  `api/auth.rs` the same way profile CRUD is restricted).

### H1.2 — Forge: agent-scoped turn entry point

- `POST /agents/:id/conversations/:cid/messages` (thin alias of
  `POST /messages` with the tenancy check against the agent owner,
  not just the session owner) so mule's wakes (H3) and cross-host
  callers address *agents*, not raw sessions. Keep `POST /messages`
  alive (ranchd's forge worker uses it — `ranch-2/crates/ranch/src/
  forge.rs`).
- `GET /agents/:id/active` → `{busy: bool, current_conversation?:
  uuid}` from the in-flight-turn marks (`agent_registry.rs:767–785`
  `begin_turn/end_turn/has_in_flight_turn`). H6's "steer vs queue"
  routing and mule's voice loop depend on this.

### H1.3 — Ranch: protocol + daemon

- `crates/ranch-protocol/src/lib.rs`: add the `AgentRegistry` frame
  family next to the workflow family (L848–892 is the request/response
  pattern to copy):

  ```
  AgentList  { req_id }                    → AgentListOk { req_id, agents: Vec<AgentInfo> }
  AgentTalk  { req_id, agent, mode: "continue"|"fork"|"new", text?, title? }
                                                   → AgentTalkOk { req_id, session, pane }
  AgentStatus { req_id, agent }            → AgentStatusOk { req_id, busy, current_conversation }
  AgentActivity { req_id, agent, kinds? }  → AgentActivityOk { req_id, items: Vec<ActivityItem> }  // H5
  ```

  (`AgentInfo {id, name, avatar, home_machine, last_active,
  conversation_count}`. `docs/PROTOCOL.md` §8 gains a `8.6 Agent
  registry (Herd H1)` section documenting these — protocol doc
  updates are mandatory per ground rule 2.)
- `crates/ranch/src/daemon.rs`: new `handle_frame` arms for the
  family; a `fn agent_talk(...)` that: (1) resolves the agent via the
  forge worker (new `ForgeJob::AgentList/AgentTalk` — `crates/ranch/
  src/forge.rs`, add to the `ForgeJob` enum alongside `Send` ~L377),
  (2) for `continue`: reattach to the agent's most recent
  conversation → `SessionsCreate {kind:"forge-chat", ref_id}` reuses
  the existing factory at `daemon.rs:1187`; for `new`/`fork`: call
  `POST /agents/:id/conversations` (with `fork_from`), then same
  factory; (3) if `text` given, immediately `ChatSend`.
- `home_machine` awareness: if the agent's `home_machine` is a
  different ranchd than the caller, H1 renders a clear "this agent
  lives on machine X — open there" status error (cross-machine
  federation is out of scope; single-user single-machine is the
  product, `visibility:'org'` is multi-*agent* not multi-*host*).
- CLI: `ranch agent [name] [message…]` (dispatch in `main.rs`,
  client path in `client.rs`): no name → dashboard Agents tab;
  name → attach the agent's conversation pane. Dashboard: add an
  **Agents** section to the session-manager screen (client.rs,
  alongside sessions) listing `AgentListOk` with `⌨ talk` action.
- **Acceptance**: from TUI, web, and mobile-native you can list
  agents and open one; `continue` reopens the same forge session (same
  `ref_id`, history replays); `fork` creates a visible sibling; a
  failed attach flashes the status bar, never dies (M8.4 pattern).

### H1.4 — Mobile surfaces

- `mobile-native`: replace `AgentsActivity.kt` (local-pi manager,
  166 lines) with the **agent registry** screen (keep the pi-manager
  as a second tab — it's a real feature): top = forge agents
  (`AgentList`), each row: avatar, name, busy dot, last-active; tap →
  `AgentTalk{continue}` → `SessionActivity` (chat pane). Keep the
  local-pi section under a "Local pi sessions" divider. `README.md`
  updated.
- `mobile/` (web/RN): same screen as a route (the app already lists
  sessions; add an Agents tab mirroring the TUI dashboard section).
- Supabase mirror: `supabase/migrations/0008_agents.sql` — `agents`
  mirror table (`id, machine_id, name, avatar_url, home_machine,
  visibility, last_active`) so a phone on another network sees the
  registry even when a machine is offline; ranchd syncs it on change
  (the `sessions` mirror sync at `daemon.rs` state changes is the
  pattern; add the same for agents). RLS like 0001.

### H1.5 — Mule sees agents

- Mule's `agents` table is mule-local *workflow-agent config*; keep
  it, but add `external_ref TEXT` (mule migration `0011`) so a mule
  agent can point at a forge agent id. H3's `forge_conversation` step
  (H3.2) makes this the integration point. No API change needed in H1.

**Phase H1 demo checklist**: create agent "Iris" (profile: default
coding profile) via TUI; talk to her from phone; kill the phone app,
re-open, `continue` resumes mid-conversation; fork her; from TUI,
`ranch agent iris "what are you up to"`; registry visible on web.

## 5. Phase H2 — Forge durable-core refactor

**Goal: forge's core becomes pi-durable — literally. A Node harness
process runs `@earendil-works/pi-durable` (pinned, H0.1) with our
`durable-pg` Postgres backend (H0.2); forge-api (Rust) becomes the
API/tenancy/SSE/memory layer that drives the harness over IPC.
Conversations and tasks are rows; durability comes from pi-durable's
checkpoints, not from filesystem replay or process babysitting. We
write almost no harness semantics ourselves — that's the decision
(H0.3).**

> **The architecture (per H0.3's decision, 2026-10-06):** Node runs
> pi-durable; Rust keeps tenancy/API/SSE/memory/sandbox and drives
> the harness over a unix-socket IPC. We do NOT reimplement
> pi-durable in Rust. The sub-phases below state the contract each
> capability must meet — implementation is pi-durable's, wiring is
> ours.

### H2.0 — The harness runtime (`~/src/forge/harness/` + Rust client)

Two new pieces (decided in H0.3 — pi-durable is the runtime, not a
reference):

- **Node harness process** (`~/src/forge/harness/`, TypeScript,
  committed to the forge repo, baked into the forge image next to
  forge-tools):
  - deps: `@earendil-works/pi-durable` (pinned, H0.1), `durable-pg`
    (H0.2), `pg`.
  - `src/main.ts`: boots one Harness on the `storage-pg` backend;
    registers forge's **extensions** (pi-durable extensions = named
    bundles of prompt sections/tools/hooks/tasks; conversations
    store *names, not code* — tool/prompt definitions stay
    hot-swappable without DB migrations).
  - `src/ipc.ts`: a **unix-socket JSON-RPC server** for forge-api:
    `submit(conversationId, requestId, entryDraft)` (exactly-once
    turn submission — pi-durable `submissionByRequest` dedup),
    `steer(taskId, text)`, `abort(taskId, tree)`,
    `createConversation(config)`, `forkConversation(from, at)`,
    `documentGet/put`, `timerSet/clear`, plus an **event push
    channel** (harness → forge-api JSON lines) forwarding
    task-state transitions and step events so forge-api fans them
    out on the existing SSE/bus without polling.
  - Self-supervising: on boot it runs pi-durable's `resume()` to
    find unfinished tasks. The forge machine config gains a
    `forge-harness.service` (`Restart=always`, same pattern as
    `forge-api` — `lab/ct/forge/configuration.nix:177`).
- **Rust client** `crates/forge-harness-client/` (thin, sqlx-free):
  typed wrappers over the IPC socket + reconnect loop. `AppState`
  gains `harness: HarnessClient` (`api/mod.rs`).

**Stays in Rust (unchanged)**: tenancy/auth (`api/auth.rs`), the
HTTP surface, SSE fan-out (`api/sse.rs`, `bus.rs`), memory tables +
pgvector (H4), sandbox + executor (`sandbox.rs`, `tool_executor.rs`)
— tool calls flow: harness extension → forge `POST /tools/execute`
(`api/mod.rs:439`; the same callback bridge the `forge-tools`
extension uses today) → sandbox. One enforcement point for tenancy,
allowlists, and the policy hook (H2.5/H3.5) survives the refactor.

**Deleted** (the payoff): `resume.rs` (544), `session_replay.rs`
(996), the pi-subprocess management in `agent_registry.rs`
(spawn/keepalive/idle-clock; ~900 lines shrinks to a small
in-flight-turn tracker behind H1.2's `/agents/:id/active`),
`session_manager.rs`'s 30-min cleanup + `lib.rs` `cleanup_task`, and
the hand-rolled turn driver (`api/turn.rs` `drive_turn*` L191–238).
pi-durable owns turns, resume, compaction, timers, and documents.
Net: forge-api loses its harness event loop and its process
babysitting entirely.

### H2.1 — The turn, re-stated

New turn flow (replaces `api/mod.rs:294` `dispatch_message` +
`api/turn.rs`):

1. `POST /messages` (and `POST /agents/.../messages`, H1.2):
   tenancy check (unchanged) → `harness.submit(conversation,
   request_id, draft)` (request_id minted by forge-api; retries
   deduped by pi-durable's submission semantics) → 202 with the
   submission id.
2. Inside the harness: pi-durable checkpoints the turn task at every
   step, committing through `storage-pg` atomically; tool calls go
   out via the extension → forge `/tools/execute` → sandbox →
   result back; interrupted tool calls rerun only when declared
   `replay: "safe"`, otherwise the model is *told* the call was
   interrupted — pi-durable semantics, we implement none of it.
3. forge-api consumes the harness event stream, publishes
   `turn_ended` / `agent` meta on the existing bus/SSE **unchanged
   in name and payload** (ranchd's `crates/ranch/src/forge.rs`
   consumer), and updates the in-flight-turn marks behind
   `GET /agents/:id/active` (H1.2).

Same external contract as today; different owner underneath.

### H2.2 — Ownership trees, background tasks, subagents

All pi-durable natives — our work is exposure, not implementation:

- **Ownership/abort**: pi-durable aborts bottom-up through the
  ownership tree. We expose it: `POST /sessions/:id/interrupt`
  (`api/sessions.rs`, route `api/mod.rs:958`) gains `?tree=1`
  (default: whole tree) → forge-api `harness.abort(taskId, tree)`.
- **Background tasks** (`background: true`, conversation-owned,
  outliving the turn): kinds `timer` (H2.3), `compaction` (H2.4 —
  built into pi-durable), `research` (H5), `subagent` (below).
- **Subagents**: a pi-durable task/tool that creates a child
  conversation owned by the calling task (smaller model / tool
  subset from args), waits (checkpointed) or detaches. Exposed as
  the `spawn_subagent` tool in the forge harness extension. UI: the
  child conversation shows under the parent in
  `GET /sessions?parent=…`; SSE events `subagent_started/ended`
  (H5/H6 render nested panes; H2 guarantees the API + events).
- **Acceptance**: kill -9 the harness mid-turn → restart →
  conversation consistent (pi-durable's own
  `harness-tasks-recovery.test.ts` semantics, run against
  `storage-pg` in H0.2's suite); abort a parent mid-subagent →
  child task dies; a background timer survives its parent turn
  completing.

### H2.3 — Durable timers

pi-durable's task timers are the primitive (research §3.2); our
surface:

- Harness IPC `timerSet(conversationId, {at|cron, prompt})` →
  forge-api `POST /conversations/:id/timers`, `GET/DELETE …/timers`.
  Firing = pi-durable timer task submits a turn with the prompt
  (system-role entry "timer fired: …").
- This is where **forge's scheduler lives now** — the
  `docs/SCHEDULED-AGENTS.md` systemd-timer pattern is deprecated;
  H2.6 ports the existing heartbeat agents onto it.
- **Acceptance**: set a 30-second timer on a conversation; kill -9
  the harness twice during it; the prompt still fires exactly once
  (submission dedup); repeat timer honors cron after a restart.

### H2.4 — Compaction, reset/handoff, history search

Mostly pi-durable built-ins (compaction is a background task;
summary lands at the next turn boundary; nothing deleted —
`search_history` finds everything). Our exposure:

- The threshold heuristic in `api/mod.rs:349–361` (300k chars/4)
  moves into the harness: the harness extension enqueues compaction
  tasks from a per-conversation config document.
- **`reset(note)`**: `POST /sessions/:id/reset {handoff_note}` →
  `harness.reset(conversationId, note)` (pi-durable `reset()` from
  a handoff note; transcript preserved).
- **History search**: `GET /sessions/:id/history?q=` — forge-api
  queries the `durable_entries` table directly (read path is
  Postgres; trigram/GIN index added by a `durable-pg` companion
  migration; pgvector memory search in H4 is a separate concern).
- The existing `POST /sessions/:id/compact` (`api/mod.rs:957`)
  stays: forge-api forwards it to the harness and awaits.
- **Acceptance**: a 500-turn synthetic session compacts in the
  background without interrupting an in-flight turn;
  `GET /sessions/:id/context` (L956) reports the reduced window;
  `reset` preserves `history?q=` results for the old segment.

### H2.5 — Hooks, prompt sections, documents (the malleable layer)

- **Documents** (pi-durable typed JSON, committed atomically with
  the transcript): harness IPC `documentGet/put` → forge-api
  `GET/PUT /sessions/:id/documents/{name}`; UIs subscribe via the
  existing SSE (new event `document_changed` emitted from the
  harness event channel). First users: `plan` (agent's plan/todo),
  `handoff` notes, H4's belief-render cache.
- **Prompt sections** (pi-durable sections, rebuilt per request,
  delta-cached by content hash): the forge harness extension
  registers built-ins — `document:name` renders a document;
  `memory:beliefs` renders the agent's active beliefs (H4 plugs in
  here; the section fetches from forge's memory API over the same
  HTTP bridge tools use); `memory:signals` (cross-agent signals,
  H4.6). Profile `system_prompt` + agent `extra_instructions`
  (H1.1) ride in the conversation config.
- **Hooks** (pi-durable hook chains, `HookRunner` L121 semantics,
  decisions memoized): our `before_tool` hook = tenancy re-check +
  `tools_allowlist` filter (agent-level, H1.1 column) + **policy
  hook** (H3.5 plugs in: evaluates mule policy rules; `ask`
  disposition creates a ranch approval chip via the F3b `notify`
  hook — `api/ranch_tools.rs:213` `NotifyBody` — and checkpoints
  the task waiting on the answer; the memoized decision survives
  resume because it's in the task checkpoint). `before_tool` can
  block → tool never runs, model gets the block reason.
- **Acceptance**: a `before_tool` hook that denies `bash` for a
  specific agent blocks it with a reason in the transcript; a
  memoized decision survives kill -9 + resume; changing a document
  mid-session changes the next turn's prompt.

### H2.6 — Migrate & cutover

- **No big bang.** Order inside the phase: H2.0 (harness + client
  built and deployed alongside; legacy path untouched) → **new
  conversations only**: sessions created after the flag flip are
  pi-durable conversations (with their `durable_*` rows); existing
  sessions keep the legacy `drive_turn` path; a shadow-parity check
  (the H1 demo script re-run against harness sessions) runs for a
  week → H2.2–H2.5 land (they only exist on the harness path) →
  **cutover**: the legacy `drive_turn` path is deleted; existing
  sessions are lazily migrated on first touch — their `messages`
  imported into `durable_entries` (one-time, logged; the importer
  reuses `session_replay.rs`'s reading logic before that module is
  deleted).
- `messages` table stays (the audit log; `tool_output`, sequence
  locks) as the *flat projection* of durable entries for the OpenAI
  surface and existing consumers; the harness's event stream keeps
  it in sync post-cutover (forge-api writes it on events). If that's
  too heavy, `messages` becomes write-only history and reads
  migrate to `durable_entries` — decide at cutover, document the
  choice in `docs/ARCHITECTURE.md`.
- **Every cutover gate**: full forge e2e suite (whatever
  `just test`/integration runs today — `crates/forge-api` has
  `api/events_integration.rs` plus external e2e) green; the ranchd
  forge worker attached to a live session survives the cutover with
  no protocol change; `docs/SCHEDULED-AGENTS.md` heartbeat agents
  re-registered as forge timers and the systemd timers retired;
  a kill -9 of *both* forge-api and the harness mid-turn recovers
  to a consistent state (the H0.2 kill-9 test, scaled up).
- Hot-upgrade note for ranchd: none (forge is its own process), but
  the ranchd forge worker's SSE reconnect logic
  (`crates/ranch/src/forge.rs`) must tolerate the longer
  fork/restart windows (harness restarts are now frequent and
  harmless by design) — add a test.

## 6. Phase H3 — Mule becomes the behavior plane

**Goal: mule can wake forge agents on conditions, run workflows that
operate *on* long-lived forge conversations, hold and evaluate
policy, and stream per-job.**

### H3.1 — Tenancy parity (forge-grade auth)

- Migration `0011_tenancy.sql`: `users` (id, email, name,
  password_hash argon2, role) + `api_keys` (SHA-256 `key_hash`,
  `key_prefix`, `restricted`, `expires_at`) — same shape as forge's
  `002_users_and_api_keys.sql` (forge is the reference).
- New `internal/api/auth.go` middleware: Bearer
  `sk_mule_<hex>` → user context on the request; admin role bypass;
  restricted keys: no agent/policy write. All `/api/v1/*` and
  `/v1/*` routes get it (wire into `cmd/api/server.go` L201–279 the
  way logging middleware is wired at L43 `middleware.go`).
  Default dev mode: no auth required when `MULE_NO_AUTH=1` (local
  single-operator; ranch's `mule_url` local setup keeps working).
- Every primitive table gets `owner_id` (default the bootstrap
  admin) + scoping in `store_pg.go` queries.
- **Acceptance**: two users cannot see each other's workflows/jobs;
  a restricted key can run but not create; `MULE_NO_AUTH` mode
  unchanged for existing ranch integration.

### H3.2 — `forge_conversation` step type (the addressing primitive)

- Migration `0012`: `workflow_steps.step_type` CHECK adds
  `'forge_conversation'`; new nullable `forge_step_ref` not needed —
  it all lives in `config` JSONB:
  `{forge_url, api_key_ref, agent_id?, conversation_id?, mode:
  "message"|"steer"|"follow_up"|"fork", content: <template over job
  input>, wait?: bool, timeout_secs?}`.
- Engine: `processForgeConversationStep` next to
  `processAgentStepWithWorkingDir` (`internal/engine/engine.go:369`):
  resolves the conversation (agent's active/latest via forge H1.2
  `GET /agents/:id/active`, or the pinned `conversation_id`, or
  forks via `POST /agents/:id/conversations`), POSTs the templated
  content, and if `wait: true` polls `GET /sessions/:id`
  (or subscribes to `GET /sessions/:id/events` SSE — mule's Go
  `net/http` can read SSE directly) until the turn ends, then returns
  the turn's assistant output as the step output. `api_key_ref`
  resolves from a new `secrets` table (or env for v1 — document the
  upgrade path in `spec.md`).
- `spec.md` + `plan.md` updated (mule ground rule: docs are the spec).
- **Acceptance**: a 3-step workflow (wasm echo →
  `forge_conversation` wait:true → wasm transform) runs against the
  forge machine; the forge agent's conversation shows the mule-originated
  message with a mule-job-tagged system prefix; cancellation
  (`DELETE /api/v1/jobs/:id`, `cmd/api/server.go:279`) mid-wait
  cancels the poll without leaving the forge turn abandoned
  (document: the turn runs on; the *workflow* gives up waiting).

### H3.3 — `wakes`: the fleet-level wake-policy engine

Ranch's trigger scheduler (`ranch-2/crates/ranch/src/triggers.rs`)
stays the **machine-local edge** (cron/event/webhook firing mule jobs
or local panes). Mule `wakes` is the **fleet superset** that can
address *any* forge agent from *any* condition, including conditions
only mule sees (job completed, memory trigger, agent signal).

- Migration `0013_wakes.sql`:

```sql
CREATE TABLE wakes (
    id              VARCHAR(255) PRIMARY KEY,
    owner_id        VARCHAR(255) NOT NULL,
    name            TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('cron','webhook','event','job','memory','agent_signal')),
    spec            JSONB NOT NULL,      -- per-kind, below
    target          JSONB NOT NULL,      -- {agent_id | forge_conversation config (H3.2)}
    prompt_template TEXT NOT NULL,       -- rendered with the fire payload
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    rate_limit_per_hour INT NOT NULL DEFAULT 24,  -- runaway guardrail (H5.3)
    last_fired_at   TIMESTAMPTZ,
    next_due_at     TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE wake_log (
    wake_id VARCHAR(255) REFERENCES wakes(id) ON DELETE CASCADE,
    fired_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    payload JSONB, job_id VARCHAR(255), ok BOOLEAN, error TEXT
);
```

- `spec` per kind: `cron {expr}` (reuse the cron parser semantics of
  ranch `triggers.rs:160` — port the 5-field expr to Go, or shell out
  to the same libm cron semantics; document the subset);
  `webhook {source, tag?}` (matches the ranch webhook shape
  `{source, event_tag, payload}`); `event {type}` (mule bus events:
  `job.completed`, `job.failed`, `agent.turn_ended` — the last two
  arrive via forge SSE watch jobs, H3.4); `job {workflow_id,
  status}`; `memory {agent_id, predicate}` (H4.5);
  `agent_signal {agent_id, kinds}` (H4.6).
- New `internal/wake/` package: a scheduler goroutine
  (cron lane: due-wake scan every 30 s like `engine.jobPoller`;
  event lane: an in-process pub/sub + a `POST /api/v1/wakes/fire
  {kind, source, event_tag?, payload}` ingress endpoint for webhooks
  and external event pushes). Fire = render `prompt_template`
  (Go `text/template` over the payload) → H3.2 forge-conversation
  submission (inline, not via a job — wakes are cheap; but record in
  `wake_log`; a fire that needs a *workflow* instead sets
  `target.workflow_id` and submits a job).
- API: `GET/POST /api/v1/wakes`, `PATCH/DELETE /api/v1/wakes/{id}`,
  `GET /api/v1/wakes/{id}/log`. Rate limiting: a fire within
  `rate_limit_per_hour` of the last N is dropped + logged
  (`ok=false, error='rate_limited'`) — this is the "runaway
  proactivity" guardrail from research §10.3, enforced *here*, in the
  control plane, not in the agent.
- **Ranch integration**: the existing ranch trigger → mule job path
  (`triggers.rs:248`, `mule.rs:503`) is unchanged; additionally,
  ranchd's `WebhookEvent` handling (daemon.rs) gains an optional
  "forward to mule wake" config (`daemon.toml [wakes] mule_forward =
  true`) so a single webhook can drive both local panes and fleet
  agents. (Ranch-side frames: none new — it's an HTTP side-effect in
  the mule worker.)
- **Acceptance**: cron wake messages a forge agent on schedule;
  webhook fire (curl to mule) wakes an agent within 2 s;
  `job.completed` on a workflow wakes a "reporter" agent; a malicious
  1 Hz event source is throttled by `rate_limit_per_hour` and the
  throttle is visible in `wake_log` + (H5) the Activity View.

### H3.4 — Per-job WS subscriptions

- `internal/api/websocket.go`: the hub is a global firehose
  (`BroadcastJobUpdate` L123). Add client-side filtering:
  `WebSocketMessage` (L17) gains `{"op":"subscribe","job_id":"…"} /
  {"op":"unsubscribe","job_id":"…"} / {"op":"subscribe_all"}`;
  `WebSocketClient` (L24) carries a `subscribed map[string]bool`;
  broadcast paths check it (default when nothing is subscribed:
  everything — backward compatible with the ranch mule worker's
  panes, `ranch-2/crates/ranch/src/mule.rs:302`).
- `JobStreamer` (L258) uses the same filter when tailing.
- **Acceptance**: two WS clients, one subscribed to job A, one to job
  B; updates to A arrive only at client 1; the ranch workflow pane
  still gets its job's stream; regression test in
  `internal/api/middleware_test.go`'s suite (add `websocket_test.go`).

### H3.5 — Policy engine (custom rules + auto-review)

The control plane that keeps autonomous action trustworthy (research
§2.2 / §7.3). Mule owns the store + evaluator; forge's hook (H2.5)
is the enforcement point; ranch is the approval UI.

- Migration `0014_policies.sql`:

```sql
CREATE TABLE policies (
    id          VARCHAR(255) PRIMARY KEY,
    owner_id    VARCHAR(255) NOT NULL,
    scope       TEXT NOT NULL CHECK (scope IN ('global','agent','org')),
    agent_id    VARCHAR(255),           -- for scope='agent'
    rules       JSONB NOT NULL,         -- array, typed below
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    version     INT NOT NULL DEFAULT 1, -- bumped on every change; audit
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

- **Rule shape** (typed JSON, validated in `internal/validation/`):
  `{id, description, action: {tool: "bash|write|edit|web|email|*",
  match?: <glob over tool input fields>}, sensitivity: "low|normal|
  sensitive", disposition: "allow|deny|ask", reason?}`. Sensitivity
  tiers are the dots model (research §2.2): `sensitive` data →
  named-recipient rules; destructive/irreversible or money → always
  `ask`; credentials/money transfer → `deny` with
  "hand-back-to-human" reason.
- **Evaluator** `internal/policy/`: given `(agent_id, tool, input)`
  → fetch active policies (global + org + agent, most-specific wins,
  conflicts resolve toward *more restrictive*) → deterministic check
  (WASM-optional but v1 is Go: pattern matching) → returns
  `{verdict: allow|deny|ask, rule_id, reason}`. Ambiguous/
  high-stakes rules with `reviewer: true` spawn a **review agent
  step** (a mule agent, cheap model, `agent/x` call) that sees the
  action + the user's rules + the agent's stated intent, and returns
  a verdict with reasoning; its decision is cached keyed by
  (rule_id, action-hash) — the memoized-decision semantics H2.5
  expects.
- **API**: `GET/POST/PATCH/DELETE /api/v1/policies`,
  `POST /api/v1/policies/evaluate` (the forge hook calls this;
  synchronous, p99 target < 50 ms for deterministic rules,
  review-agent path unbounded but the hook checkpoints the task —
  the agent *waits*, like a `ranch_ask` answer).
- **Forge side** (belongs to H2.5 but specified here): the policy
  hook = a `before_tool` that calls `/api/v1/policies/evaluate`;
  `deny` → blocked tool with the reason; `ask` → forge calls the
  F3b `notify` hook (`api/ranch_tools.rs:213`) which becomes a
  ranchd `AgentAskRequest` (ranch-2 `protocol lib.rs:761`) carrying
  the rule id + action summary; the answer returns via the existing
  F3a result path; forge un-blocks (or denies on TTL/lost) and
  records the decision memo in the task checkpoint.
- **Agent-editable only through approval**: `PATCH /api/v1/policies`
  from an agent's mule API key requires a `human_approved: true`
  claim — which mule only issues after a mule *workflow* with a
  `ranch_ask`-style human step ran (v1: an operator endpoint
  `POST /api/v1/policies/{id}/approve-version` from a non-restricted
  key; the "agent drafts rules, human approves" loop is the H6
  activity card). Agents can *propose*: `POST /api/v1/policies/
  proposals`.
- **Acceptance**: a policy `deny` on `bash` matching `rm -rf`
  blocks the tool with the reason visible in the transcript; an
  `ask` policy on `write` to `/etc/**` produces a ranch approval
  chip on all clients (TUI status bar + mobile push, H6.1);
  answering "yes" lets exactly that action through (memoized: a
  retry of the same call doesn't re-ask); changing a policy bumps
  `version` and the audit log (new `policy_audit` table or `wake_log`
  style table) records who/what changed it.

## 7. Phase H4 — Memory & learning (the differentiator)

**Goal: agents have persistent, inspectable, versioned memory —
episodic + semantic + shared — written by reflection workflows that
are themselves mule workflows; belief changes surface as approval
cards.**

### H4.1 — Forge: memory tables (pgvector)

Migration `017_memory.sql` (after H2's 020-range numbering; adjust to
the real sequence):

```sql
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE episodes (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id       UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    conversation_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    task_ref       TEXT,                -- durable task id when episode is task-scoped
    summary        TEXT NOT NULL,       -- "did X, outcome Y, user feedback Z"
    feedback       JSONB,               -- {explicit: "…", implicit: "edited the diff", ts}
    embedding      vector(1024),       -- Qwen3-Embedding-4B dim (confirm; H4.2)
    source         JSONB NOT NULL,     -- {conversation_id, seq_range} provenance
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX episodes_agent_time ON episodes(agent_id, created_at DESC);
CREATE INDEX episodes_embedding ON episodes USING hnsw (embedding vector_cosine_ops);

CREATE TABLE beliefs (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id         UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    scope            TEXT NOT NULL DEFAULT 'agent' CHECK (scope IN ('agent','org')),
    kind             TEXT NOT NULL,    -- 'preference'|'fact'|'procedure'|'constraint'
    content          TEXT NOT NULL,    -- "user prefers Conventional Commits"
    embedding        vector(1024),
    confidence       REAL NOT NULL DEFAULT 0.5,
    source_episodes  UUID[] NOT NULL DEFAULT '{}',   -- provenance
    watch            JSONB,            -- memory-trigger predicate (H4.5)
    status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending','active','forgotten','superseded')),
    superseded_by    UUID REFERENCES beliefs(id),
    version          INT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX beliefs_agent_active ON beliefs(agent_id) WHERE status = 'active';
CREATE INDEX beliefs_embedding ON beliefs USING hnsw (embedding vector_cosine_ops);

CREATE TABLE memory_acl (          -- org/shared memory access (scope='org' rows)
    agent_id UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    org      TEXT NOT NULL,
    access   TEXT NOT NULL CHECK (access IN ('read','write')),
    PRIMARY KEY (agent_id, org)
);

CREATE TABLE agent_signals (       -- cross-agent bus (H4.6)
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    from_agent UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    to_agent   UUID REFERENCES agents(id) ON DELETE CASCADE,  -- NULL = org broadcast
    kind   TEXT NOT NULL CHECK (kind IN ('handoff','insight','request','watch')),
    payload JSONB NOT NULL,
    embedding vector(1024),
    consumed_by UUID[] NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

- `scope='org'` beliefs are the "shared/team memory" tier: readable by
  agents with `memory_acl` read on that org, writable only via
  reflection workflow with provenance (research §10.4: memory writes
  go through reflection, never directly from tool results).
- Embedding dimension: whatever Qwen3-Embedding-4B via Bifrost
  returns (verify against `embedding.rs:71` `embed`'s actual vectors
  — forge currently stores `REAL[]` in
  `007_session_embeddings.sql`; keep those tables, they serve the
  router; H4 adds the vector columns alongside).

### H4.2 — Episode capture (turn-end, in forge)

- New `crates/forge-api/src/memory.rs` (+ module in `lib.rs`): at
  turn completion (the H2.1 step-3 hook point — *after* the
  `turn_ended` publish), a fire-and-forget task (mirroring
  `refresh_session_summary` in `api/mod.rs`):
  1. slice the turn's entries (user prompt → assistant/tool final);
  2. **deterministic first pass** (no LLM): extract structured
     fields — files touched, commands run (from `tool_input`),
     explicit feedback patterns (user said "no/wrong/actually I
     wanted…"), edits made to agent output (diff of user
     re-prompts);
  3. **cheap-model second pass** (configured profile, small model):
     one-sentence `summary` + `feedback` JSON;
  4. embed (Bifrost, `embedding.rs` client) → insert `episodes`.
- Explicit "remember this": a `memory_remember(content, kind?)` tool
  on the agent (in-process tool, forge-tools extension entry):
  inserts a `beliefs` row `status='pending'` directly (it IS
  user-instructed) and returns "recorded; pending your review".
- **Acceptance**: every completed turn produces exactly one episode
  with provenance pointing at the entry seq range; `memory_remember`
  creates a pending belief; no episode embeds leak tool *secrets*
  (redaction pass — check against `logging.rs`'s redaction list).

### H4.3 — Retrieval & prompt injection

- `GET /agents/:id/memory/search?q=&k=12&scope=agent|org` → pgvector
  cosine over that agent's active beliefs (+org if ACL read) and
  recent episodes; returns with `source_episodes` provenance.
- Prompt section `memory:beliefs` (H2.5 registry): at each turn,
  (a) top-confidence active beliefs (≤ 15), plus (b) top-K beliefs
  retrieved against the *current user message* (k=5) — rendered as
  a compact "What you know about this user" block. Section text is
  stable unless beliefs change (content-hash cache, H2.5).
- **Acceptance**: teach an agent "always sign off commits with
  Co-authored-by: Herd" via `memory_remember` + approve; next
  conversation, new session, the commit it makes carries the
  trailer; `GET …/memory/search?q=sign-off` finds it with provenance.

### H4.4 — Reflection loop = mule workflow (the dogfood)

The canonical reflection workflow ships as a real mule workflow
(`~/src/mule/examples/` + seeded via H3.1 admin API):

1. **Trigger**: a mule `wake` (H3.3) — `cron "0 3 * * *"` (nightly)
   *and/or* `event` ("N=20 new episodes since last reflection").
2. **Step 1 (wasm)**: pull the agent's un-reflected episodes +
   current active beliefs (forge memory API + `GET
   /agents/:id/memory/search` over the episode summaries).
3. **Step 2 (agent, cheap model)**: propose belief diffs as
   structured JSON: `[{op: add|strengthen|weaken|forget|supersede,
   belief, confidence, rationale, source_episodes}]`.
4. **Step 3 (wasm, deterministic)**: contradiction & duplication
   check (embed the diff; flag any existing belief with cosine >
   0.92 that asserts the opposite → mark for human review instead of
   auto-apply), confidence arithmetic (decay: `confidence *=
   exp(-λ·age)` applied to stale beliefs; λ configurable).
5. **Step 4 (agent step, or wasm for safe ops)**: submit to
   `POST /agents/:id/memory/beliefs/proposals` (new forge endpoint,
   this phase: writes `beliefs status='pending'`).
6. **Governance**: forge pushes each pending belief to ranchd as an
   approval card — reuse the F3b `notify` → `AgentAskRequest`
   path (H3.5) with card metadata
   `{kind:"memory", agent, content, rationale, source_episodes}` →
   "I've learned: you prefer brief replies. **Keep / Forget / Edit**".
   Keep → `status='active'` (+ version bump). Forget → `'forgotten'`.
- **Belief changes are reviewable forever**: `GET /agents/:id/memory/
  beliefs?status=` + audit rows (`belief_audit`: who/what
  proposed/changed, version chain). This is the "dots can help write
  Custom Rules but need approval to change them" analog, for memory.
- **Acceptance**: run the reflection workflow against a week of real
  episodes; ≥ 80% of proposed diffs are sensible to a human (manual
  review, recorded in `docs/research/` as a dated eval note); the
  approval cards render on TUI + mobile with Keep/Forget; a
  contradictory pair never auto-activates.

### H4.5 — Memory triggers (wake conditions that watch memory)

- A belief can carry `watch: {match: "text or embedding predicate",
  threshold: 0.8, cooldown_hours: 24, wake: <mule wake spec>}`.
- Implementation: at episode insert (H4.2), forge embeds → scans the
  agent's `watch` predicates (pgvector k=1 per watch, threshold
  cosine) → hit & cooldown elapsed → `LISTEN/NOTIFY` to
  `herd_memory_trigger` channel → the mule host's listener
  (`internal/wake/` new lane) POSTs the fire to its own
  `POST /api/v1/wakes/fire` (kind `memory`). (Cross-host: forge and
  mule are different processes; NOTIFY only reaches the same DB —
  so the listener lives where it can reach mule's HTTP: a small
  `notify-forwarder` goroutine in mule that polls
  `SELECT … FROM memory_trigger_queue` — a forge-written table —
  every 30 s. Simpler, restart-safe, no extra socket.)
- **Acceptance**: belief "watch for any episode mentioning 'invoice
  deadline'"; trigger an episode; the woken agent's follow-up turn
  fires within ~1 min; cooldown suppresses a second episode.

### H4.6 — Cross-agent signals

- `agent_signal(to?, kind, payload)` tool on every forge agent
  (in-process; writes `agent_signals`, embeds payload, NOTIFY).
- Recipient visibility: each agent's prompt section
  `memory:signals` (H2.5) renders *unconsumed* signals addressed to
  it (or org-broadcast to its org, if ACL allows) — consumption is
  implicit (rendered ⇒ `consumed_by += agent`).
- **Delegate learning without transcript**: `ranch_spawn`-style
  subagents (H2.2) and mule-spawned forge conversations get
  `memory_scope:'agent'` read on the *delegator's* episodes **only**
  via `GET /agents/:id/memory/search?scope=episodes&task_ref=` —
  summaries, never raw transcript (dots' "keep the information
  needed for the task" rule, research §6.3).
- **Acceptance**: invoicing agent publishes an `insight` ("PO
  template changed"); primary agent's next turn mentions it
  unprompted; a subagent spawned for a task can retrieve its
  delegator's task-relevant episode summaries but 404s on
  other conversations' transcripts.

## 8. Phase H5 — Proactivity

**Goal: agents pursue goals 24/7 — background work, proactive
(read-only) research, wake conditions — presented through one
cross-agent Activity View with guardrails against runaway
proactivity.**

### H5.1 — Proactive research (read-only by construction)

- New durable task kind `research` (H2.2 background tasks):
  created by `POST /agents/:id/research {question, scope?}` or by a
  mule wake whose target is a research task.
- **Structural enforcement** (research §2.2: "we enforce these limits
  in code"): the research task's tool registry is a *filtered*
  `ToolExecutor` — allowed: `read`, `search` (the SearXNG CLI
  skill in the sandbox, `~/src/forge/docs/SEARCH-TOOL.md`),
  `webfetch` (new in-process tool, GET-only, no cookie/cred
  forwarding), `note` (write to the task's private
  `research_notes` document). Denied (the registry literally lacks
  them): `bash`, `write`, `edit`, `web` (POST), email, any `ranch_*`
  tool, any mule tool. The harness (H2.1) receives the tool list
  per task, not per agent — the model never sees a write tool it
  could call.
- Output: a `research_report` document on the conversation + a
  suggestion card to the user (ranch `AgentAskRequest` with
  `kind:"research"`: "I looked into X. Findings: … **Use /
  Discard / Ask more**"). A research task *cannot* message anyone,
  change anything, or steer any other agent — its only exits are the
  report + the card.
- **Acceptance**: `POST /agents/:id/research` with a real question;
  the task's event trace shows zero write-class tool calls (assert in
  an e2e test that the tool registry passed to pi has no
  write/bash/edit/web-post entries); the report lands as a document;
  the card renders on all three clients.

### H5.2 — Wake-condition wiring (the always-on loop)

The full wake matrix, each row = (condition → detector → sink):

| Condition | Detector | Sink |
|---|---|---|
| cron | mule `wakes` cron lane (H3.3) | forge agent conversation / workflow |
| webhook | mule `/api/v1/wakes/fire` + ranch `WebhookEvent` (local) | agent or local pane |
| file change | forge host: inotify tailer worker on configured paths (new `watch:` entries in `/etc/forge/forge.env` or per-agent `config`) → bus event | forge timer task |
| agent turn ended | forge bus `turn_ended` (existing, `bus.rs:136`) | mule `event` wake (H3.3) |
| job completed | mule `job.completed` event | mule `event` wake |
| memory match | H4.5 watch predicates | mule `memory` wake |
| agent signal | H4.6 `agent_signals` | recipient's `memory:signals` section (pull) + optional wake (push, kind `agent_signal`) |
| user "check back in N days" | agent tool `schedule_reminder` (forge, H2.3 timers surfaced as a tool) | forge timer |

- **Acceptance**: a scripted scenario — webhook at T0 → agent turns
  at T0+2 s; that turn ends → second agent's "triage" wake fires;
  its work writes an episode → memory trigger wakes a third agent.
  Each hop is visible in `wake_log` / task history.

### H5.3 — Activity View (ranch, all surfaces)

- **Data**: daemon-side aggregation, new `Activity` in
  `crates/ranch/src/` (a poller worker with its own pipe pair, per
  AGENTS.md worker rules; polls every 5 s + pushes on local events):
  1. local: `AgentAskRegistry` pending approvals (`agenttools.rs:337`),
     `SpawnRegistry` active spawns (L89), trigger fires
     (`triggers.rs:303`), mule jobs this machine launched
     (`mule.rs:302`);
  2. forge (via new `ForgeJob` messages):
     `GET /agents/:id/tasks?state=pending,running` (H1.1's
     `/agents/:id/tasks` now real — H2's durable tasks),
     `GET /agents/:id/memory/beliefs?status=pending`,
     `GET /agents/:id/research?open=1`;
  3. mule (via new `MuleJob::Activity`): `GET
     /api/v1/wakes?enabled=true` recent `wake_log` entries.
- **Frames**: `AgentActivity`/`AgentActivityOk` from H1.3 is the
  request/attach path; the push is `ActivityPush { items: [ActivityItem] }
` broadcast (DECIDED part 2: a dedicated frame instead of
  `Meta { kind:"activity" }` — Meta carries only a `status` string and
  has no payload slot for structured items; old clients drop the unknown
  tag, so it is additive) — throttled: only on item-set change, debounced
  2 s — so clients update a timeline without polling. Plus
  `AgentPause { req_id, agent?, paused }` → `AgentPauseOk { req_id,
  results: [AgentPauseResult] }` (the pause kill-switch pair).
- **UIs** (parity, same phase): TUI dashboard tab (client.rs:
  timeline grouped by agent: 🧠 memory cards, ✅ approvals,
  🔬 research, ⏰ wakes); web Agents tab section; mobile-native
  `ActivityActivity.kt` (new, wired into `MainActivity.kt`'s nav row
  next to Agents). Each item: tap → deep-link (approval → the ask
  chip; research → its conversation; wake → the conversation it
  woke; "stop" action → task abort, H2.2 tree abort).
- **Global kill switch**: Activity View header has **Pause herd** /
  **Pause <agent>**: daemon → forge `POST /agents/:id/pause` (new:
  enqueues nothing new — timers parked, turns rejected with "agent
  paused", in-flight turns finish; `wake` fires are no-ops logged) +
  mule `POST /api/v1/wakes/pause {agent_id, paused}` (bulk
  enable/disable that agent's wakes). Per-agent + global variants;
  landed as `AgentPause`/`AgentPauseOk` frames + `ranch
  pause|unpause [AGENT]` (part 2; see §H5.3 ledger entry — the CLI
  verb is "unpause" because `ranch resume` is taken by session resume).
- **Digests**: notification policy setting (H6.1): when > N items
  accumulate, one digest notification per agent instead of N.
- **Acceptance**: with three agents doing different things, the
  Activity View on phone and TUI shows the same ordered timeline
  within 10 s of each event; pausing the herd stops all wakes for
  ≥ 5 min (verified via `wake_log`); resuming picks up cron wakes at
  the next boundary (no catch-up storm — `triggers.rs:266`
  `catch_up` semantics applied to mule wakes too).

## 9. Phase H6 — Presence & voice

**Goal: the agent is reachable and auditable on every surface, and
can be talked *to* with your voice — including a phone-call-style
loop.**

### H6.1 — Mobile agent notifications (with actions)

Today `mobile-native/app/src/main/java/dev/ranch/android/Notify.kt`
(notice engine, 284 lines) reacts to `Meta`/`Chat` frames:
pane-turn-end + `⚠`-error rows (`onMeta` L115, `onChat` L139,
`postLocal` L247). Upgrade:

- **Agent-scoped notifications**: the daemon's agent registry (H1)
  + the Activity worker (H5.3) let notifications carry an agent
  identity: title = agent name + avatar (downloaded to
  `cacheDir/avatars`), body = the event. Kinds:
  `turn_end` (existing, re-tagged), `error` (existing `⚠`),
  `approval` (H3.5/H4.4 cards — **default-on**, actionable),
  `research` (H5.1 cards), `memory` (belief cards), `task_done`
  (background task completed).
- **Notification actions** (the dots "message you with decisions that
  need you" surface): each actionable notification carries PendingIntents
  that post frames through the foreground service's Realtime socket
  (`MonitorService.kt` → `Monitor.kt` → `RelaySession.send`):
  approvals → `AgentAskAnswer`; research → `AgentAskAnswer`
  (Use/Discard); task notifications → "open" (`SessionsSelect`),
  "stop" (new `TaskAbort` frame — daemon → forge tree abort),
  "redirect" (opens a fork: `AgentTalk{mode:"fork"}`).
- **Per-agent mute/priority**: `Prefs.kt` gains a per-agent settings
  map (mute, priority high/normal/quiet → quiet items only enter
  digests); a "notifications" screen (new, off SettingsActivity or
  the Agents screen) lists agents with toggles.
- **Expo/web parity**: web gets a browser Notification API mirror of
  the same policy (the former Expo app's `mobile/lib` notification code
  was the reference — now in git history; the live policy module is
  `NotifyPolicy.kt`).
- **Acceptance**: phone locked, agent's approval card fires →
  notification with agent avatar + Keep/Forget buttons → tap Keep
  while phone stays locked → belief activates (verified in forge);
  muted agent produces no toasts but its items appear in the
  Activity View.

### H6.2 — Voice input on mobile

- Push-to-talk in the chat pane (`SessionActivity.kt`'s send row,
  1860 lines — add a mic button next to send): Android
  `SpeechRecognizer` (on-device, no new deps) → transcribed text →
  normal `ChatSend`. Fallback path: `MediaRecorder` 16 kHz mono
  → `POST /v1/audio/transcriptions` (forge proxy, `voice.rs:128` →
  Parakeet) → text → `ChatSend`. The forge-proxy path is the one we
  prefer for consistency with the voice loop (H6.3).
- Settings: `voice_input: recognizer|forge-stt` in Prefs.
- **Acceptance**: with airplane mode, recognizer mode works; online,
  forge-stt mode works (and works for accents the recognizer
  mangles — verify with one real sample, note in README).

### H6.3 — Voice loop v1 (push-to-talk "call")

A mule-orchestrated job, per research §7.4: mule owns the async
boundary (a call is a long-lived interaction a conversation loop must
not block on).

- **Mule side**: new workflow `voice-loop` (seeded, H3):
  steps: `wasm: audio-in` (holds the RTSP/WebRTC-less transport: v1
  is **WebSocket audio** — phone/TUI stream 16 kHz PCM frames to
  mule `ws://mule/voice/{job_id}/in`) → on VAD silence ≥ 600 ms
  (wasm, energy-based; no dep) → `wasm: stt` (chunked POST to forge
  `POST /v1/audio/transcriptions`, streaming chunks flushed at
  pauses; full utterance at turn end) → `forge_conversation`
  (H3.2, `mode:"steer"` — each utterance steers the agent mid-task,
  the research doc's "treats each utterance as a steer message") →
  agent's final assistant text → `wasm: tts` (POST forge
  `POST /v1/audio/speech` → Kokoro, chunked by sentence, streamed to
  the out socket) → back to listening. Job stays RUNNING until
  hung-up.
- **Surfaces**: `ranch call <agent>` (CLI: opens the WS, plays
  audio via `aplay`/`pulseaudio` on this box — the TUI gets the same
  call UI as a pane: audio in via mic if available else
  key-typed-text fallback so TUI parity is real, not fake);
  mobile-native: "📞" button on the agent row (AudioRecord →
  WS → the same mule job; MediaPlayer for out).
- **Barge-in v1**: while TTS plays, the in-socket VAD keeps running;
  user speech mid-TTS → stop playback, start the STT chunk (simple
  barge-in; true full-duplex simultaneous speech is v2 below).
- **Acceptance**: 5-minute call, 20+ turns, agent steered mid-task
  at least once ("while you do that, also check X") and it did;
  kill the mule worker mid-call → the job is marked failed, both
  surfaces show "call dropped", the conversation is intact (turns
  committed).

### H6.4 — Full-duplex barge-in (v2, still *in scope*)

- Move VAD+STT to always-on dual channels (in and out simultaneous),
  endpointing by RNN-free heuristic (silence + energy ratio) in wasm;
  the agent harness treats overlapping user speech as `steer`
  (H2.1 already supports mid-turn steer via `AgentSend`
  `delivery:"steer"` → pi RPC `steer`).
- Latency budget: TTS first-chunk < 400 ms (Kokoro sentence-chunked
  streaming — measure in `spikes/`), STT partials < 800 ms.
- **Acceptance**: interrupt the agent mid-sentence 10 times in a row;
  10/10 the TTS stops within 150 ms and the interruption is heard;
  no echo feedback (out-channel gain zeroed while local mic hot).

### H6.5 — Specialist-dot provisioning UX

- `ranch agent new` wizard (TUI, then mobile "＋ Agent"): name,
  avatar (pick from local files → upload), profile (from
  `GET /profiles`), tools allowlist (checkboxes over the profile's
  tools), memory scope (agent/org + org id), home machine (this
  machine), default wakes (offer: nightly reflection, heartbeat
  cron, none) → one call each: forge `POST /agents`, mule
  `POST /api/v1/wakes` ×N, supabase mirror sync (H1.4).
- Credentials scope (the dots "own identity, credentials" tier):
  `agents.credentials_scope` JSONB (H1) stores `{env_refs: […]}` —
  named credential slots resolved by the sandbox (`sandbox.rs`) at
  tool-execution time, **never into the model's context** (research
  §2.2 credential isolation). v1 supports one slot type: an env-var
  secret ref from forge's `/etc/forge` secret store; the "secure
  sign-in pauses the model" flow is H6.5's explicit out-scope
  remainder → *no*: replace with a concrete task —
  **web-auth tool**: `web_login(url)` tool opens the URL in a
  browser on the user's machine (via the user's own browser, not the
  agent's), the agent's sandbox gets a session cookie through a
  broker step that requires the user's `AgentAskAnswer` approval —
  the model never sees the password field. Implement as an in-process
  tool + approval chip, documented in `docs/design/agent-builder.md`.
- **Acceptance**: from phone, create a "billing" specialist
  (restricted tools: no bash, no edit; org memory write; heartbeat
  wake daily 07:00); within 24 h it has run ≥ 1 proactive turn;
  its credentials are invisible in any transcript row
  (grep audit test).

## 10. Cross-cutting work

### 10.1 — Protocol frame inventory (ranch-2, consolidated)

All new frames land in `crates/ranch-protocol/src/lib.rs` (Frame enum
L19) and are documented in `docs/PROTOCOL.md` §8 (new §8.6–8.10).
Every new client-facing frame must be handled (at minimum: ignored
with a warning log) by all four clients: TUI (`client.rs`), web
(`web/`), mobile-native (`Monitor.kt`), and the raw test client
(`tools/relay-test.mjs` updated to print them).

| Frame | Phase | Direction | Purpose |
|---|---|---|---|
| `AgentList` / `AgentListOk` | H1 | c↔d | agent registry |
| `AgentTalk` / `AgentTalkOk` | H1 | c→d / d→c | open/continue/fork a conversation |
| `AgentStatus` / `AgentStatusOk` | H1 | c↔d | busy + active conversation |
| `AgentActivity` / `AgentActivityOk` | H5 | c↔d | activity pull |
| `Meta { kind:"activity", … }` | H5 | d→c | ~~activity push (debounced)~~ — **superseded**: H5.3 part 2 shipped the dedicated `ActivityPush { items }` frame instead (`Meta` carries only a `status` string, no payload slot — the decoder drops unknown tags so older clients are unaffected; see the H5.3 part 2 ledger note + PROTOCOL §8.6) |
| `TaskAbort { task?, agent?, all? }` | H2/H6 | c→d | ~~tree abort (Activity "stop", call "stop")~~ — **not shipped** (H5.3 part 3a + H6.1 deliberately shipped the v1 surfaces without stop/abort actions — the Activity item's [open] and the task_done Open/Fork buttons are v1; see the H6.1 ledger note + the Closeout's deferrals list) |
| `AgentAskRequest { card?: {kind, agent, …} }` (extension of L761) | H3/H4/H5 | d→c | approval/memory/research cards |
| `VoiceCall { req_id, agent, in_url?, out_url? }` / `VoiceCallOk` | H6 | c↔d | ~~call transport session (mule job id)~~ — **not shipped** (H6.3 chose host/phone ⇄ mule direct WebSockets; no ranch frame, no daemon in the media path — see PROTOCOL.md §8.7 + the H6.3 ledger note) |
| `AgentPause { agent?, all? }` / `AgentPauseOk` | H5 | c→d | kill switch |

Ranch CLI additions (`main.rs` dispatch + `client.rs`):
`ranch agent [name] [msg…]` (H1), `ranch call <agent>` (H6.3),
`ranch pause|unpause [AGENT]` (H5.3 — "unpause" because `resume`
is session-resume), `ranch agents new` wizard (H6.5). `ranch
activity` (this list's original H5 entry) was **not shipped** —
the TUI Activity section + both mobile Activity UIs shipped
instead; the CLI one-shot timeline print is a deferred item.

### 10.2 — Doc updates per phase (checklist, part of the phase)

- **ranch-2**: `docs/SPEC.md` (agent entity §; Activity View §;
  voice §; policy §), `docs/PROTOCOL.md` §8 additions,
  `docs/design/` — new: `herd-overview.md` (concept architecture +
  sequence diagrams for: a woken turn, an approval flow, a
  reflection loop, a voice call), `agent-builder.md` (H6.5
  credential flow), `mobile-native/README.md` (agent notifications,
  voice, Activity), `docs/history/` dated build-log entry per phase
  (the M-series pattern).
- **forge**: `docs/ARCHITECTURE.md` (rewritten for the durable core:
  turn worker, checkpoints, hooks, memory; the old
  harness/executor split is archived into the build log),
  `docs/API.md` (all H1/H2/H4 endpoints), `docs/SCHEDULED-AGENTS.md`
  (marked superseded by forge timers + mule wakes, kept as history),
  new `docs/HERD-DURABLE-DECISION.md` (H0.3),
  `docs/OPERATIONS.md` (pgvector, timer worker ops, pausing).
- **mule**: `spec.md` (wakes, policies, forge_conversation, voice,
  tenancy), `plan.md` (phase status), `README.md` (behavior-plane
  framing).
- **AGENTS.md (this repo)**: the Herd section gains a pointer to this
  file; the "Open decision" line (Rust vs Node sidecar) was resolved
  2026-10-06 (pi-durable upstream — H0.3) and the note is updated.
- **research doc**: leave untouched (it's the concept record); add a
  pointer at its top: "Implementation plan: `docs/PLAN-HERD.md`".

### 10.3 — End-to-end demo scripts (the real acceptance gates)

Each ships under `tools/herd-demo/` (this repo) as a shell script +
checklist; CI-optional, run by hand per phase:

1. **`herd-demo-h1.sh`** — agent CRUD + talk/continue/fork from all
   three surfaces.
2. **`herd-demo-h2.sh`** — kill -9 forge mid-turn ×3, resume,
   transcript consistent; subagent spawn/abort; timer across
   restart; compaction of a synthetic long session.
3. **`herd-demo-h3.sh`** — cron/webhook/job wakes; a policy
   `deny` + an `ask` (phone chip) end to end.
4. **`herd-demo-h4.sh`** — week of episodes → reflection → approval
   cards → active belief changes behavior in a fresh session.
5. **`herd-demo-h5.sh`** — proactive research card; memory trigger
   wake; Activity View parity TUI↔phone; herd pause.
6. **`herd-demo-h6.sh`** — voice call with mid-task steer; barge-in
   ×10; specialist provisioning from phone.

### 10.4 — Deployment & operations notes

- **forge on mini** (`lab/ct/forge` machine): migrations run at boot
  (`lib.rs:71`
  `sqlx::migrate!`) — H2's cutover needs a deploy window (old code
  must not read new tables before it knows them; migrations are
  additive-only until cutover, then a final migration may drop the
  legacy projection). Upgrade path = forge's self-update endpoint
  (`docs/OPERATIONS.md`); the hot-upgrade `ranch upgrade` discipline
  from AGENTS.md applies to any *ranch* daemon that is itself an
  agent under test — agents working in ranch panes must use the
  Upgrade frame, never a self-restart.
- **mule**: the fleet mule runs on the new private **`mule` ALC
  machine on mini** (H0.4); `mule-public` stays the public demo,
  untouched. The new listeners (wake scheduler, memory-trigger
  poller, voice loop) are goroutines in the same mule binary
  (`cmd/api`) — no new deploy unit; they live inside that one
  machine.
- **Supabase**: new migrations `0008_agents.sql` (H1.4) +
  notification fan-out stays Realtime broadcast (frames ride the
  existing `machines:<id>` topic — no new channels; agent cards are
  `AgentAskRequest` frames, already relayed).
- **Costs/scale**: explicitly out of scope per the research doc's
  posture — no budget/limit logic beyond the anti-runaway rate
  limits (H3.3/H5.3).

## 11. Risks & mitigations (research §10, operationalized)

| Risk | Mitigation (task) |
|---|---|
| Harness runtime (decided: pi-durable, not a reimplementation) | H0.2 conformance run on `storage-pg`; H0.3 kill-criteria recorded (conformance failure, IPC latency vs H6.4 targets, upstream churn) — any of these reopens the decision formally |
| pi/pi-durable API churn | H0.5 pins everywhere; `pi_agent.rs`/`pirc/pibridge.go` are the only pi surfaces — keep them thin; pi-durable semantics frozen at the pinned commit in H0.1 |
| Runaway proactivity | H3.3 `rate_limit_per_hour` (control-plane enforced), H5.3 kill switch + digests, H5.1 read-only-by-construction research |
| Prompt injection at fleet scale | H4.4: memory writes only via reflection workflow with provenance + contradiction gate; belief diffs always reviewable; H3.5 policies live outside the constrained agent (separate control plane — the dots safety lesson, research §2.2) |
| Supabase relay latency/reliability on the wake/notification path | wakes fire machine-local (mule→forge direct), Supabase only carries *presentation* frames; H5.3 digests bound the blast radius; self-hosted Supabase is the documented fallback |
| Voice latency quality | H6.3 push-to-talk first with measured budgets; H6.4 barge-in with explicit latency targets; both in scope, sequenced |
| Ranch core regression under all this new surface | every new ranchd worker follows AGENTS.md invariants (own pipe pair, bounded non-blocking writes, hot-upgrade survival); H5.3/H6 land after H2 cutover is stable ≥ 1 week of normal use |
| H2 cutover data corruption | new-conversations-only rollout (H2.6) for a week; legacy path kept until gates pass; `messages` remains the auditable flat projection |
| Single-host concentration (everything on mini, per user directive) | all durable state sits in the two Postgres VMs (forge db in the forge machine; mule db in the mule machine); the `forge-backup` service already pg_dumps on a timer (`lab/ct/forge/configuration.nix:135`) — mirror it in `lab/ct/mule`; machine rebuilds are atomic (reconcile recreates from the new image); user surfaces (ranchd on user machines) are unaffected even if mini is down — agents just sleep until wake |

## 12. Open questions (only the ones that block)

Everything else in the research doc's open-questions list was
answered in-line or by directive (multi-tenancy: build on forge's
existing users/keys; storage: Postgres, confirmed by pi-durable's own
backend story; voice v1: push-to-talk; scope: all of it, no MVP
scoping; **deployment: user directive 2026-10-06 — everything on
mini, everything as Apple Linux containers; `ha` is out of scope**;
**fleet mule: the new private `mule` machine from H0.4**;
**Q1 (2026-10-06): use pi-durable upstream directly — no Rust
reimplementation**; see H0.3 for the recorded decision and its
kill-criteria). Nothing open blocks starting H0.

---

*Status ledger (updated as phases land):*

| Phase | Status | Notes |
|---|---|---|
| H0 | ◐ in progress | H0.1 ✅ (vendor @ ae92585d = pi-durable 1.0.4, digest notes written), H0.2 ✅ (forge `57d5035b`: `durable-pg` Postgres Storage backend — vendor conformance suite 23/23 green, kill-9 ×2 green, README w/ PG-vs-sqlite benchmarks; note: npm dist ships 20 conformance cases, vendored src has 23 w/ scan `order`), H0.3 ✅ (decision record at `forge/docs/HERD-DURABLE-DECISION.md`), H0.4 ✅ (lab fleet live: mule machine @ 10.10.199.29:8091, forge rebuilt with pgvector + harness + models.json model catalog; six reconciles 2026-10-10/11 — see the Closeout's "Lab fleet cutover" item), H0.5 ✅ code-side (extension pins + ranchd drift warning; image rebuilds ride H0.4's reconcile) |
|| H1 | ✅ complete (mobile live smoke pending) | H1.1/H1.2 ✅ (forge `418c7bce`: migration 016 agents table + sessions.agent_id, api/agents.rs — CRUD w/ owner-or-admin tenancy + restricted-key gate, /conversations list+create (fork_from = message copy w/ sequence reset), /tasks stub, /active from in-flight marks; 10 integration tests), H1.3 ✅ (ranch-2 `eca27c1`: AgentList/AgentTalk/AgentBusy/AgentActivity frames + PROTOCOL.md §8.6, daemon agent_talk (continue/new/fork + home-machine guard + first-message), `ranch agent [name] [msg]` CLI w/ `--new` legacy preserved, dashboard Agents section, one_shot 30s read timeout; live smoke a–f green vs local forge; forge `8dd668ad` adds conversation working-dir fallback for unprivileged hosts), H1.4 ✅ (ranch-2 `a81a40a` Supabase `agents` mirror (0008) + ranchd sync on AgentListOk/agent_talk; `ecec532` mobile-native agent-registry screen (busy dots, AgentTalk continue → SessionActivity; pi manager kept under a divider; release APK built); `170d606` Expo Agents tab (registry section + AgentTalk attach; profile builder intact); live phone/web behavior not yet smoke-tested), H1.5 ✅ (mule `b2aa75e`: migration 0011 agents.external_ref VARCHAR(255) + index, wired through primitive.Agent + PGStore CRUD w/ sql.NullString; build/vet/test green) |
| H2 | ✅ complete (lab harness live 2026-10-11; remaining: heartbeat re-register + forge-public vendor re-sync) | H2.0 ✅ (part 1: forge `147486a9`+`ed282592` — `harness/` Node workspace over pi-durable 1.0.4 + durable-pg, RPC+events sockets, recovery-on-boot, 6 vitest green incl. real-socket spawn e2e; lab `86a3f1d`+`b5a6601` — `forge-harness.service` + pin; **pending: lab image build + first-boot verification**). Part 2: forge `7d530311` — `crates/forge-harness-client` (IpcClient + EventStream, reconnect w/ backoff, ResyncRequired, 15 tests) + forge-api `HarnessState` (disabled-mode default, event consumer → same bus events + in-flight marks, interrupt→abort `?tree=`, migration 017 `sessions.durable_conversation_id`; full suite green, clippy 0, fmt clean, verified on disk). H2.1 ✅ (forge `5ba85553` — `FORGE_HARNESS_MESSAGES=1` flag routes NEW sessions to the harness: attach at creation w/ legacy fallback, `POST /messages` → `submit` (fresh request_id, 503 on failure), TurnEnd → assistant projection into `messages` + `message` bus event (dedup via migration 018; tool rows unchanged via `recording.rs`), forks stay legacy, no per-request deltas yet (documented in `harness/CLIENT.md`); live acceptance test green: real harness child + faux provider, full turn → row + bus events + idle. 142+10+15+3 tests, clippy 0, fmt clean, verified). H2.2 ✅ + H2.3 ✅ (forge `002f89da` — `spawn_subagent` tool (harness/src/subagent.ts, pi-durable child-task ownership: foreground task-ownership + background anchor, replay-safe, requestId dedup), child forge sessions via migration 020 `sessions.parent_session_id` + `GET /sessions?parent=`, bus/SSE `subagent_started`/`subagent_ended` on the parent stream, interrupt ?tree= verified; durable timers (harness/src/timer-store.ts: PG table in durable schema, atomic fire-claim `UPDATE…WHERE fired_at IS NULL`, boot catch-up, cron re-arm), IPC timerSet/clear/list, API `POST/GET/DELETE /sessions/:id/timers` (deviation from spec's /conversations/ noted); harness 13/13 vitest incl. SIGKILL-exactly-once timer + abort-parent-kills-child + recovery-mid-subagent; Rust full suite green, clippy 0, fmt clean, verified on disk). H2.4 ✅ + H2.5 ✅ (forge `89032406` — compaction threshold moved to the harness (config-doc defaults 300000/4 = legacy heuristic; background task, LiveDoc in-flight dedupe), IPC compact/reset/compactionStatus, `POST /sessions/:id/reset {handoff_note}` (pi-durable reset, transcript preserved), `GET /sessions/:id/history?q=` (guarded pg_trgm GIN on durable_entries.record via history-index.ts, portable fallback), `/context` post-compaction; documents API `GET/PUT /sessions/:id/documents/{name}` + `document_changed` bus/SSE; prompt sections document_plan/document_handoff + memory_beliefs stub (H4 slot); before_tool allowlist hook (tools_allowlist via createConversation, block reason visible to model, marked H3.5 policy slot); harness 19/19, Rust full suite green, clippy 0, fmt clean, verified on disk). H2.6 ✅ (forge `73c67650` — cutover: `pi_agent.rs` + `api/turn.rs` (drive_turn) + `resume.rs` + `session_replay.rs` + their tests DELETED; the harness is the only turn path. `dispatch_message` = atomic migration claim + user-row INSERT in ONE transaction (claim+insert make the import's `sequence < cap` fetch race-free under concurrent writes) → `ensure_migrated` → `submit` → 202; lazy migration (migration 021 `harness_migrating`/`harness_migration_at`, 10-min staleness self-expiry, losers poll ~30 s → 503 InProgress) imports legacy transcripts via new `importEntries` RPC (one commit; tool rows folded into toolCall blocks + `pi.tool-result`; orphan healing; placeholder skip; caller-row exclusion by sequence cap) and stamps the session; `FORGE_HARNESS_MESSAGES` is now a kill switch (default ON, `=0` → every write 503 before the row lands; session creation never fails on the harness); `resync_unprojected` — on `ResyncRequired` the event consumer projects every durable `pi.assistant` entry without a `durable_projection` claim, so a `turn_end` fired while the consumer was down (harness kill -9) loses no assistant row; OpenAI surface rewired (`run_agent_turn` = ensure_migrated + submit + wait_for_turn_text). Gates: `dual_kill9_recover_mid_turn` (legacy session, kill -9 the harness mid-turn on process 1, fresh API+harness recovers the turn on process 2 — exactly-once prompt/import, one answer row; needed the resync rescan to pass — the resumed turn completed while app_2's event client was still redialing) + `concurrent_writes_migrate_exactly_once`; forge 19/19 test binaries, clippy 0, fmt clean, harness 20/20 vitest; ranch-2 `crates/ranch/src/forge.rs` SSE reconnect tests added (replay dedup via `last_seq` high-water mark + heartbeat/lagged no-ops; 42 tests, clippy 0, fmt clean). Docs: forge `docs/ARCHITECTURE.md` rewritten, `docs/SCHEDULED-AGENTS.md` systemd-timer pattern DEPRECATED in favor of `POST /sessions/:id/timers` (curl in the banner), AGENTS.md/OPERATIONS.md/CHANGELOG updated. **Lab note:** `~/src/lab/ct/forge-public/vendor/forge/` is a pre-H2.0 vendor snapshot (no `harness/`, legacy docs) — re-vendor it from post-`73c67650` forge before lab CTs run the new scheduling/docs; lab changes stay uncommitted in ranch-2. **Pending live (next session):** re-register the lab heartbeat agents as forge timers (`POST /sessions/:id/timers`) and retire their systemd timers on the lab machine.) |
| H3 | ✅ complete | H3.1 ✅ (mule `93aad23` — migration 0012 users+api_keys mirroring forge 002 shape, idempotent bootstrap admin (fixed default UUID for deterministic backfill), `internal/api/auth.go` Bearer sk_mule_ middleware (SHA-256 key lookup, 401/403, restricted-key POST/PUT/PATCH/DELETE 403 on agents/workflows/jobs/policies incl. sub-paths), MULE_NO_AUTH=1 pass-through w/ synthetic admin, owner_id on all primitive tables + backfill + owner scoping in store_pg/manager/pkg-job stores; full `go test ./...` green in BOTH auth modes, verified on disk; docs/TENANCY.md). H3.2 ✅ + H3.4 ✅ (mule `62f409e` — migration 0013 admits `forge_conversation` step type; `engine/forge.go` processForgeConversationStep (addressing: pinned conversation → GET /agents/:id/active → POST conversations; fork mode; mule-job-tagged prefix; wait via SSE turn_ended w/ deadline + ctx-cancel; output = turn assistant text) + config validation in the step-create route; fake-forge integration tests incl. 3-step workflow + cancel; H3.4: websocket.go per-client subscribe/unsubscribe/subscribe_all w/ default firehose preserved (ranch mule worker compat) + acceptance matrix test; spec.md/plan.md updated; full suite green in BOTH auth modes, verified on disk). H3.3 ✅ (mule `e09ddc0` — migration 0014 `wakes` + `wake_log` (owner-scoped, due-scan index); `internal/wake/` = cron/webhook/event/job live kinds + memory/agent_signal inert H4 stubs; 30 s cron-lane scan + bounded EventBus event lane; fire path: prompt_template render (missingkey=error) → rate_limit_per_hour guardrail (drop + log `error='rate_limited'`, research §10.3) → workflow `SubmitJob` or H3.2 `engine.ForgeSubmission.PostMessage` (fire-and-forget, `[mule wake <id>]` prefix) → wake_log + cron next_due_at advance in one txn (no double-fire on rescan); engine hook: `SetJobEventSink` on `processJob` terminal paths → `job.completed`/`job.failed`; `POST /api/v1/wakes/fire` ingress (webhook {source, tag?} / event {type}); owner-scoped CRUD API, wake = proactivity primitive so H3.1 write-restriction does NOT apply (documented); cron = faithful port of the ranch 5-field subset (*, */n, lists, ranges, UTC, dow 0-6, no ?/L/W/names); spec.md "Wakes" + plan.md notes; fake-forge/real-engine tests: cron-once, webhook fire, job.completed end-to-end, rate limit, workflow target, owner isolation, inert kinds — full `go test ./...` green in BOTH auth modes, verified on disk; ranch-2 `969d03c`: `[wakes] mule_forward` webhook → mule `/wakes/fire` forward (throwaway thread, errors logged + dropped).). H3.5 part 1 ✅ (mule `f4185c5` — migration 0015 `policies` + `policy_audit` + `policy_memo`; `internal/policy/` evaluator (action_hash = sha256(tool + canonicalized sorted-key input); memo-first with rule-agnostic most-restrictive memo; active-policy scope owner-global + agent; most-specific agent>org>global, in-scope conflicts deny>ask>allow, tie → earliest rule id; no match → allow default; deterministic verdicts upserted to policy_memo so resume never re-asks; v1 deterministic floor for reviewer:true — the review-agent upgrade path documented, not built); two-leg matching (glob over rendered field=value lines + raw-substring for bash — exact semantics in spec.md "Policy Engine"); owner-scoped API: CRUD (version bump + audit rows), `POST /policies/evaluate` (sync, deterministic-only, open to restricted keys — documented: they can evaluate but not write), proposals + `approve` (non-restricted only, `approved-proposal` audit row); 14 new tests (deterministic matrix, memo + loosen-block, hash stability, owner isolation, version/audit, restricted-key 403-on-write/200-on-evaluate, proposal→approve, validation 400s, NO_AUTH path); spec.md + plan.md; full `go test ./...` green in BOTH auth modes, build/vet/fmt clean, verified on disk). H3.5 part 2 ✅ (mule `0293244` — `POST /api/v1/policies/memo`: record a human-approved `ask` decision as a memoized allow (same `upsertMemo` path the evaluator uses — no drift), open to restricted agent keys like evaluate (auth carve-out documented), 400 on verdict not allow/deny; `TestPoliciesMemo` (ask → memo → memoized allow, different action still asks, restricted key may write) + tenancy route test; spec/plan updated. forge `a0e8b52b` — the `before_tool` policy hook at the H2.5 slot: `FORGE_POLICY_URL` opt-in (env read once at extension build), evaluate → allow/deny/ask; deny blocks with the rule's reason; ask → `POST /sessions/{id}/policy-ask` (ranch_tools.rs: owner-or-admin tenancy gate, SAME `RanchToolQueue` + `ranch_tool_request` SSE event with tool `policy_ask` and payload `{rule_id, reason, tool, input}`, long-poll bounded by the 60 s relay TTL → `decision: allow|deny|expired`); on allow a best-effort `/policies/memo` write so the same action is memoized, never re-asked; fail-closed on any error/timeout (block + console.error — a policy deployment must never be bypassed); `ranch_*` relay tools exempt (their own consent); `policyAgentId` rides `CreateConversation` → `forge.meta` → boot re-install (v1 agent-id convention: the FORGE agent id, `session:<forge session id>` fallback); harness `test/policy.test.ts` 6 tests (in-process fake mule + fake forge: no-op, deny, ask→allow+memo, ask→expired fail-closed, memoized allow, unset env). ranch-2 `9ba9a71` — the `policy_ask` lane: the forge worker's `ranch_tool_request` handler drives it through the EXISTING `AgentAsk` machinery (no protocol change — rule context in the `question` text, choices Allow/Deny, `free_text: false`), polls `/agent/ask-status` ~2 s cadence to a ~55 s deadline (under forge's 60 s TTL so a slow human surfaces as forge's `expired`), relays allow/deny/timeout to `/ranch-tools/{id}/result`; design/agent-tools.md bridge section documents the lane. Gates: mule `go test ./...` green in both auth modes; forge `cargo test --workspace` green + clippy/fmt clean + tsc/vitest 26/26; ranch-2 cargo test green (2 pre-existing ranch-vt replay failures, missing machine-local fixture), clippy/fmt clean, verified on disk) |
| H4 | ✅ complete | (see the H4 row in the phase table above: H4.1 `c9d4ee54`, H4.2 `4a2127e8`, H4.4 `dd737acf`+ranch-2 `9bcb5c3`+mule `c217760`, H4.5 `619918e8`+mule `e9f08ec`, H4.6 `bccb7b4c`; pgvector install still pending → memory DB tests run under the skip contract) |
| H5 | ✅ complete | H5.1 ✅ (forge `883ace25` read-only research + `research_report` card; ranch-2 `d8d5e83` card lane). H5.2 ✅ (forge `bbdde8e1` — file-watch wake, turn-end → mule event-wake forwarder, `schedule_reminder` tool, `scripts/herd-wake-scenario.sh` manual fleet scenario). H5.3 ✅ — part 1 (forge `96c31aef` pause/resume + real `GET /agents/:id/tasks`; mule `36d0b9e` `wakes/pause` + `wakes/activity`); part 2 (ranch-2 Activity View daemon: poller worker, `ActivityPush` frame, `AgentPause` kill switch + `ranch pause\|unpause`); part 3a (ranch-2 `7b86899` TUI Activity section); part 3b (ranch-2 `ad8ce74` mobile Activity UIs, both clients). Stop/abort action deferred (v1 = [open] only — see §10.1 `TaskAbort`). Full notes: the H5 row of the phase table above. |
| H6 | ✅ complete | H6.1 ✅ (ranch-2 `9f734ba` — agent-scoped mobile notifications with action buttons + per-agent mute/priority, both phone clients; `TaskAbort`/Stop deferred). H6.2 ✅ (ranch-2 `feb68e9` — voice input: forge-Parakeet push-to-talk default + on-device recognizer, mic button, Settings; Expo web hold-to-talk). H6.3 ✅ (mule `8b402f0` voice-loop state machine + `voice_call` step + in/out WebSockets, `examples/voice-loop/`; ranch-2 `f32a00f` `ranch call` + mobile CallActivity + `one_shot` stale-daemon guard; ZERO new ranch frames — PROTOCOL §8.7). H6.4 ✅ (mule `e8f4c86` full-duplex barge-in v2: ratio endpointing, flush control <150 ms, out-queue liveness; ranch-2 `d4a07df` echo guard + `.spike/voice-latency/` spike — real STT/TTS numbers owed). H6.5 ✅ (forge `3a943d6f` secrets + credentials scope + web_login broker + grep-audit; ranch-2 `aa24619` `ranch agents new` wizard + mobile ＋Agent `AgentProvision` form + web_login card lane). Full notes: the H6 row of the phase table above. |



| Live ops 2026-10-16 | ✅ lab fleet recovery + memory search decode fix | ALC apiserver outage orphaned all 8 machines; recovered by killing the orphan runtimes + `machine run` (data intact). (1) **Embeddings live**: Bifrost back up at the ALC-internal forwarder (10.10.199.29:8080); `FORGE_EMBEDDING_URL` added to the host-side `forge.env` (model default `embeddings/qwen3-embedding-4b`, 2560 dims). (2) **memory/search latent decode bugs fixed** — forge `45dfb375` + `635735c9`: `(Episode|Belief, f32)` tuple decode → `FromRow` flatten on the hit structs (sqlx bound column 0 as a composite record named `Episode`), and the cosine score is FLOAT8 from pgvector → `::float4` cast for the f32 bind. Both only surface once an agent has embedded episodes/beliefs — the H4.4 reflection pull step 500ed live the first time Bob had one. Hot-deployed to the lab forge via `/opt/forge-hotfix` + `/run/systemd/system/forge-api.service.d` drop-in (image rebuild rides the lab-repo flake bump to `635735c9`, lab `b713bd0a`). (3) **mule daily jobs fixed**: both pointed at forge agent `ea978f3a` (deleted 10/11 — the "billing agent is gone" residual noted at H2 rollout); re-pointed to Bob (`0a5ad23d`) in `wasm_modules.config` + the `forge_conversation` step; both workflows now complete (agent-reflection full pull→agent→check→submit green; the agent step needed a `Default` pi provider entry for the forge `/v1` endpoint in the host-side `pi/models.json`, + the stale `bitfrost.botnet` URL corrected). The genai memory tool needed role `mule` + pgvector on the mule DB (created live; made durable in lab `2153aa0d` via `withPackages ps.pgvector` + `mule-pg-user` oneshot). Stray `test-agent-30m` wake disabled (rate-limited fire-drops re-fire on every 30 s scan — scheduler should back off dropped fires; noted, not fixed). (4) **ranch-mini image v4**: built on the forge VM (alcbuild2 had lost NAT egress + its stopped-state held the mDNS hostname), shipped via the shared volume, `lab-alc-reconcile` recreated the machine on the new image; running binary cksum-verified against the pinned source. Six PRE-EXISTING memory_tests failures remain (H4.4 card-push semantics, H4.6 redaction expectations — red on CI since ~10/11, unrelated to the decode fix; CI fmt+clippy now green after forge `9e2dfc10`/`f9f77485`). |
| Live ops 2026-10-16b (voice calls) | ✅ mic calls fixed end-to-end | "Call an agent" never answered — three stacked failures, all live: (1) **mule `EncodeWAV` wrote its RIFF/WAVE/fmt/data magics byte-reversed** (big-endian constants through `LittleEndian.PutUint32` → ffmpeg saw `FFIR…WIVE…fmt …dat ` and 415'd every mic upload; the STT hop is degraded-not-dead so the phone sat in silence — never worked against real forge: fakeforge never parsed WAVs and wav_test pinned the same wrong constants; mule-ai/mule#131, lab `ct/mule` pinned at `9f94474`); (2) **the cloud voice tunnel was gone** — CF deleted the named tunnel during the public-edge cleanup and anonymous quick tunnels are now rejected outright ("Unauthorized: Tunnel not found"), while `voice_gateways` rows still pointed at the dead 10/11 `trycloudflare` hostname → new named tunnel **ranch-voice** (id `16a95558-3522-44f2-b945-bdcaf7086dc4`) with DNS **voice-lab.butler.ooo** → voice-gw nginx:8443; token via pool env `~/.lab-alc/data/voice-gw/voice-gw.env` + entrypoint wrapper `run-with-env.sh` (cloudflared 2026.10.0 moved `--token` under `tunnel run`; alc.toml entrypoint updated; gateway rows re-pointed); (3) **edge fn lacked `/api/v1/workflows/{id}/steps`** → cloud-mode calls couldn't find the voice_call workflow at all (ranch-2 `e2e9d0e`, deployed). Degraded hops now emit an out-socket `{"notice": …}` frame (mule) rendered as ⚠ by `ranch call` and the Kotlin call screen (ranch-2 `3944c6b`) — no more unexplained silence. Verified: synthesized utterance → VAD → Parakeet STT → Bob → "2 plus 2 is 4." + Kokoro TTS over LAN and wss://voice-lab.butler.ooo. |
| Live ops 2026-10-16c (fleet rebuild) | ✅ ALC network recovery + durable deploy | Debugging surfaced a latent Apple Container bug: **two `container-network-vmnet` NAT daemons** (an orphan from the 10/15 `container system start` raced the fleet-wide stop/start), corrupting per-VM WAN egress while LAN stayed fine — victims were exactly the VMs never restarted since (mule, alcbuild2; the latter still stopped + hostname-locked, flagged for `machine delete` + re-run). Recovery: `container system stop/start` + full delete/recreate via `lab-alc-reconcile` onto a fresh 192.168.67.x subnet — all 8 machines + 22 containers re-verified (forge 2 agents, mule 4 wakes/4 workflows, STT 200, memory search 3 hits, bifrost 101 models, vaultwarden bw-syncs, butler-edge + voice-lab healthz 200, ranchd relay broadcasting). **Mule now runs the durable image binary `mule-9f94474`** (WAV fix baked; hotfix drop-in retired); mic-to-mic via `wss://voice-lab.butler.ooo` answered "2 plus 2 is 4" + TTS in 7 s on the durable path. Lab repo: `289e8b7f` (voice-gw `tunnel run --token` form, mule flake bump). Known-cost notes: `container stop/start` does NOT re-lease container IPs after a bridge-subnet change — containers must be deleted and reconciled; forwarder ports live (socat supervisors track machine IPs via state files). |
| Live ops 2026-10-17 (call UX) | ✅ feedback + answer-the-call | Phone call “eventually just said call dropped socket closed” — nginx saw **zero** phone requests (gateway rows verified → old APK on the device still dialed the dead host; new APK needed either way). Shipped the feedback trio (mule PR #133, stacked on #131; ranch-2 `77bea55`): **(1) answer-the-call** — in-socket attach fires a greeting prompt turn (agent literally picks up: “Hi, thanks for calling — this is Bob…”) which doubles as **prefix-cache warm-up** (system prompt + tools + history reach the model while the caller settles; first real utterance is a cache hit); a job nobody dials never greets; speech during the greeting buffers (no steers) and runs as a normal turn after. **(2) `{"user":…}`** frame echoes the caller’s own words live; **(3) `{"state":…}`** frame (deduped listening/utterance/thinking/speaking) drives the client status line. Kotlin `CallActivity` + CLI `voicecall.rs` render both; spec + wire docs updated; new Go test (pre-attach silence, greeting turn counted, lane ordering). Live-verified on the lab VM over the public tunnel: greeting @3.2 s, “Hey Bob, what is 2 plus 2?” echoed in the user lane @7.9 s, answer + TTS @9.8 s. (TTS “0 KB” in the harness was a Blob-measurement artifact — forge speech returns 60 KB PCM fine.) mule VM runs the hot-deployed branch binary; image swap rides the PR merge. **Action: user installs the 10-17 APK** (`mobile-native/…/app-release.apk`). |
| Live ops 2026-10-17b (greeting async) | ✅ redesign | “4 s seems too short — some initial prompts can take over 5 m. Make a simple, async, cancellable process.” The greeting had been a blocking pollHop on the Run loop (a 5-min first prompt = 5 min of dead air; TurnTimeout would kill the call). Now: background goroutine fired on in-socket attach; caller talks immediately (`{"state":"greeting"}` lane); **turnMu serializes agent-turn waits** (assistant rows have no request id — concurrent PostAndWait waiters could settle on each other’s late replies; queueing also stops user POSTs landing mid-greeting-turn server-side); cancellation is SPEAKING-level + one-way (first VAD speech/typed line/barge-in mutes + flushes; the agent turn still completes server-side, so the cache warm-up stands; hang-up kills; 10-min wait cap). Job output: `greeting: pending|answered|cancelled|failed`. Tests + live NOWAIT run on the lab (public tunnel): greeting @0.4 s → muted by first speech (flush @0.5 s) → user lane @4.3 s → answer @6.1 s, greeting never surfaced; normal call greets @2.6 s. mule `52c92ce` (PR #133), ranch-2 `05aaa39` (Kotlin greeting status + new APK 10-17 06:55). Note: repeated test calls accumulate greeting prompts in Bob’s voice conversation — harmless; reflection tidies. |
| Live ops 2026-10-17c (voice model) | ✅ per-call model picker | “When I call Bob instead of using Spark’s Qwen3.8-27B I want it to use the Laguna model from vram.” Forge profiles are the menu (Laguna (dev) = `llamacpp/laguna-xs-2.1` @ vram:8080 — the visalgo server on the RTX 3090; Spark = SGLang @ .174). Mule (PR #133 `c11ea28`): `CallConfig.Model` + `POST /jobs input_data.model` override (the engine already threads job InputData into `StartCall`); `ensureModel` fuzzy-matches the label against `GET /profiles` and PATCHes `/sessions/:id` (forge’s switcher → live pi-durable `setModel`) once per call, INSIDE the turn-wait lock before the greeting; unknown label → ⚠ `model:` notice listing options, call proceeds on default; api_key untouched (redacted in API; lab models are keyless). ranch-2 `0c6a586`: CLI `ranch call AGENT --model laguna`; Kotlin call screen gains a persisted “voice model” spinner (forge `/v1/models` — cloud via edge fn [redeployed, GET /v1/models added], direct via configured forge base; picker is best-effort-hidden on failure); `input_data:{model}` rides the job POST. voice-gw nginx (live via exec + lab repo): `location = /v1/models → forge`. Verified live: NOWAIT public call with `model=laguna` → journal “call model set to Laguna (dev) (llamacpp/laguna-xs-2.1 @ 10.10.199.146:8080)”, answer @5.2 s (0.7 s first-token on the 3090); tests green (fuzzy match, one-shot PATCH, notice fallback, precedence). New APK 10-17 07:20. |
## Closeout (2026-10-10)

All Herd phases H0–H6.5 are complete. The status ledger above is the
per-phase record (commit hashes in every row); this section is the
plan-completion record: what was not accepted live on this box, what
was deliberately deferred, and the decision that can still be re-opened.

### Owed acceptances (cannot run on this box — unblock commands)

- **pgvector on forge's Postgres: DONE 2026-10-10** — `sudo pacman -S pgvector` (0.8.6) installed + `CREATE EXTENSION vector` on the scratch PG; the 11 previously-skipped H4 memory tests now run and pass. This exposed a real bug — pgvector ≥ 0.8 caps HNSW/IVFFlat at 2000 dims vs our 2560-dim embeddings; fixed in forge `9ebdbd47` (index fallback in migration 022: HNSW where allowed, else filtered sequential scan; full suite 332/332 green).
- **H0.4 lab reconcile: DONE 2026-10-10/11** — the fleet mule machine is live at `10.10.199.29:8091` (secrets in `~/.lab-alc/data/mule/mule.env` + Vaultwarden `mule-private` folder); the lab forge CT carries the restored voice backends + H0.4 pgvector + the Herd harness; this box's `daemon.toml` is repointed (forge `10.10.199.29:8110`, mule `10.10.199.29:8091`). The full cutover story — including the two real bugs it exposed (the mule DSN anomaly, the harness model catalog) — is in the next item. Supabase migration 0008 (agents table) applied via the lab's `supabase-agent-token` on 2026-10-10. **User verification guide: `docs/VERIFY-HERD.md`.**
- **Lab fleet cutover (2026-10-10/11) — six `lab-alc-reconcile`s on mini, three image rebuilds of the forge machine**: (1) **mule DSN anomaly** — the aarch64 mule binary ignores the DSN database name and writes all data to the `postgres` db inside its VM (the amd64 build honors it; root cause unresolved — accepted). `lab/ct/mule/configuration.nix` now aligns with reality: DSN `/postgres`, `backup.sh` dumps `postgres` (was the empty `mule` db — backups had been silently 371 bytes), `db-init.sh` restores `-d postgres`. (2) **forge harness startup race + model catalog** — `FORGE_HARNESS_SOCKET`/`FORGE_HARNESS_EVENTS_SOCKET` are baked into forge-api's NixOS config (env + `After=`/`Wants=` forge-harness + an `ExecStartPre` 60 s socket wait; `lab/ct/forge/configuration.nix`), so a recreated machine can no longer run an api that 503s every generation with "harness unavailable (disabled mode)". Forge `6d6f7ff9` + `cabc64a1`: `createModels()` starts with an EMPTY provider map and pi-durable receives `runtime.models` prebuilt, so the harness is the only registration site — it now registers the pi-ai builtins plus the machine's `models.json` (default `/root/.pi/agent/models.json`, `FORGE_HARNESS_MODELS` overrides) via `createProvider` + the builtin api implementations, filling the provider-scoped model fields (`api`/`provider`/`baseUrl`/`type`) that `createProvider` passes through (`harness/src/main.ts` `applyModelsJson`, `test/models-json.test.ts`). (3) **thinking-fallback projection** — forge `f10ac6a5`: small reasoning models (laguna-xs-2.1 via llama.cpp) answer entirely in `reasoning_content` (pi-ai `thinking` block, empty `content`), so `extract_assistant_text` falls back to the thinking text when no text blocks exist — without it mule's `forge_conversation` step failed with "ended without an assistant reply" on turns that had visibly answered. Mule PR #130 (reflection seed fixes + `0016_voice_step.sql`) merged 2026-10-11 (admin override of the repo's PR-only branch policy — the one merge this plan still needs a human to ratify). End-to-end evidence: the `billing-daily` mule workflow fires a real forge turn through the lab LLM proxy (Laguna), the agent thinks, calls tools (`web_login` correctly blocked by the `tools_allowlist` — H1.1/H3.5 live on the wire), and the assistant row projects into `messages` (re-fire the job for the completed-job record).
- **Mid-conversation model switch (2026-10-11, forge `7d491926`, image `cd2a64d7`)** — the per-session model switcher used to apply only at conversation creation: an attached durable conversation kept its creation-time model forever (the H2.6 documented limitation), so "switching" billing to spark was a silent no-op. Now: harness `setModel` RPC (`Conversation.configure({model})` in its own commit, effective from the next provider request) + `conversationModel` RPC (drift detection via `agent()` resolution); `PATCH /sessions/:id` rebinds the attached conversation to the effective model (post-update override ?? profile) and a no-op re-save on a drifted conversation still heals it (conversations switched before this deployment). Live-switch failure returns 502 saying the DB row is recorded but the conversation is not — no more silent no-ops. Live acceptance on the lab: cleared billing's override (live switched laguna), re-set spark (live switched spark; `conversationModel` RPC confirms `proxy/spark/qwen3.8-sglang` on the real harness socket) — the agent the user believed was on spark now actually is, and mule's `billing-daily` (which reuses the active conversation) follows it. Test-infra fixes in the same commit: agent-pause test seeds its provider + provider_id (schema drift vs the shared test PG), and `test_helpers::auth_post` retries 429 per RETRY_AFTER (the H3 auth token bucket — burst 5, 1/s — raced any test registering >2 users in under a second; 5 test files updated, 333 forge-api tests green). Also: the lab's own ranch machine (`ranch-mini` VM) was registered (machine name `lab`, via the `register_machine` RPC) and its `daemon.toml` landed in the data pool `/Users/jbutler/.lab-alc/data/ranch-mini/` — it had booted local-only since Sept 29 because that file never existed (ranch-mini-dirs failed its ExecStartPre); heartbeats + relay now live. Note: the machine was registered under `owner@ranch.local` and then moved to the Google account (`jeremiah.butler@jbutler.dev`, the phone's sign-in) by a one-off `UPDATE machines/machines_info SET user_id` via the sbp_ management API — the phone's account is the account of record for the fleet.
- **Mobile UX: three agent flows + full agent builder (2026-10-12)** — user: “pi agent created nothing / how do I tell a one-off pi from a forge chat from a herd agent / the full agent builder you skipped”. Root cause of “nothing appeared”: the ranch-mini ALC image was built without the `pi` binary, so `SessionsCreate kind=pi` failed server-side (“spawn pi: No such file or directory”) and the app never surfaced the `Error` frame (the “creating pi session…” status hung forever). Fixed three ways: (1) ranch-mini flake now bakes `pi` 0.84.4 (mirrors the forge flake derivation) + sets `RANCH_PI_BIN` on the ranchd service; image rebuilt on the forge machine and reconciled onto the VM; (2) the app now surfaces `Error` frames for `sc-…` create req-ids in both the home “new agent” dialog and the Agents screen (a failed create is visible, not a hang); (3) the three flows are now labeled as such in every entry point: “π one-off chat — pi on THIS machine” (SessionsCreate kind=pi → local pi pane on that machine), “🤖 one-off chat — lab forge model” (SessionsCreate kind=forge → forge-backed conversation), and “＋ New agent” on the Agents screen = **herd agent** (registry row via the `ranch agents new` core: profile, memory scope, tools, wakes). The mobile `NewAgentActivity` is now the full builder (v2): profile picker (`ProfileList`), inline new-profile form (`ModelCatalog` + `ProfilePut`), and the tools-allowlist checkbox set (KNOWN_TOOLS; unchecked = all tools, the H1.1 empty-allowlist convention). The daemon proxy frames (ProfileList/ProfilePut/ModelCatalog) already existed from H6.5 — only the app was missing them. APK builds clean; on-device + on-lab smokes are the next step.
- **Public edge removal (2026-10-14, lab `f50ca75a`)** — user: “all of the
  public stuff should be removed; we got rid of it, no longer needed.”
  Removed the whole public surface: the NPM ALC container (the
  *.butler.ooo TLS edge on the 10.10.199.28 alias), `ct/npm`,
  `ct/forge-public` + `ct/mule-public` (vendor trees), the
  `lab-npm-alias` launchd oneshot, `mac-alc-forward.sh`'s NPM
  auto-sync (the conf/DB rewrite + nginx-reload pass), and the stale
  ranch-demo/mule-public doc references. Live on mini: container
  stopped + deleted, data pool `/Users/jbutler/.lab-alc/data/npm/` +
  the image tarball removed, alias IP off en0, oneshot booted out,
  darwin system re-switched (the plist is gone from the current
  system). The lab is LAN-only now: every workload's only reachability
  path is the socat forwarder on 10.10.199.29. Two adjacent things
  deliberately kept (flagged to the user, not asked for):
  the ha-edge alias + its pinned `.221:443` forward (ha.botnet's
  TLS edge — also public-shaped), and the router-side public NAT
  forwards themselves (unreachable from here; user confirms they're
  already gone). Note: with NPM gone, `vault.butler.ooo` no longer
  resolves — Vaultwarden access is now LAN-only if it still is
  used at all.
- **Spark agentic profile (2026-10-14)** — user: “VRAM isn't actually
  running laguna; create another forge profile that uses our spark
  qwen3.8-27b model, that's better for agentic work.” The lab forge's
  only profile was `Laguna (dev)` pointing at
  `http://10.10.199.146:8080/v1` (the vram llama.cpp box that isn't
  running it). Created profile **`Spark (Qwen3.8-27B)`**
  (id `f5f3f12c`): provider `proxy`, model `spark/qwen3.8-sglang`,
  `base_url http://10.10.199.174:8001/v1` (SGLang DIRECT — the same
  bypass as the lab pi panes, because Bifrost v1.6.4 400s pi's spark
  requests), `api_key none`, working dir `/forge/work`. Rebound the
  `test` agent's `primary_profile_id` to it. First live turn 400'd
  (“provider API error”) — the forge's `models.json` had
  `reasoning: true` on the spark entry (pi then sends reasoning
  params SGLang rejects, and SGLang's always-on
  `reasoning_content` would have been swallowed as thinking anyway);
  set `reasoning: false` in the forge data-pool `models.json`
  (`/Users/jbutler/.lab-alc/data/forge/pi/models.json` + the live
  `/root/.pi/agent/` copy, harness restarted). Verified: new agent
  conversation → `SPARK PROFILE LIVE` via the sandboxed bash tool
  (exit 0) + the model self-identifies as Qwen. The old `Laguna
  (dev)` profile still exists but is unbound — delete it when you're
  happy with spark. (Also observed: the forge's `agents` table now
  holds ONLY `test` — the billing agent `ea978f3a` is gone, presumably
  deleted through the new mobile ✕ flow; the mule `billing-daily`
  wake has no target agent anymore.)
- **H2 lab rollout** — the lab heartbeat agents still run on
  systemd timers. **Moot (verified 2026-10-14):** the forge-public CT
  no longer exists on mini and no legacy heartbeat agent-timers
  remain in the lab config; nothing to re-register or retire.
  The forge-public vendor re-sync is likewise moot (no forge-public
  consumer left on the fleet).
- **Forge img7 + ranch-mini image v3 (2026-10-14)** — the two
  outstanding image rebuilds:
  (1) **Forge img7** (`lab` `2f48ca86`/`aec9ea4c`/`441b1b0c`, source
  `0a18df68`): rebuilds the forge ALC machine with the four new
  inter-agent tools (`notify_user`, `list_agents`, `send_message`,
  `create_agent` in `forge-ext.ts`) and the `forge-sandbox-base`
  oneshot that bakes `/forge/sandbox/base/` from live NixOS binaries
  (sh, bash, coreutils incl. `timeout`, + a stub `/etc/debian_version`
  to pass the forge's rehydration check) — the sandbox base now
  survives VM recreation instead of the earlier manual `cp -a`. Live
  verification on the recreated machine: `list_agents` returns the
  agent registry from inside a turn, and a sandboxed bash call
  (`POST /sandbox/sessions/:id` → `echo IMG7 VERIFIED`) exits 0.
  Two operational fixes landed on the same machine: the test-agent
  profile's `llamacpp/laguna-xs-2.1` had `contextWindow: 81920` in
  `models.json` while the llama.cpp backend only serves 16384 — turns
  died with `exceed_context_size_error` 400s because pi never
  compacted; corrected to `contextWindow: 16000 / maxTokens: 4096`
  (source `/Users/jbutler/.lab-alc/data/forge/pi/models.json` + live
  copy, harness restarted) so auto-compaction stays under the
  backend's cap.
  (2) **Ranch-mini image v3** (`lab` `ef5dfddf`): the `pkgs.bash`
  flake fix (pi's bash tool spawns `sh`, which was absent from the
  NixOS systemd service PATH → every tool call `spawn sh ENOENT`) is
  now BAKED into the image instead of the rw-nix-store workaround;
  built on the forge machine (the alcbuild2 nix-daemon access
  problem avoided — `NIX_CONFIG="sandbox = false"` for the pi
  `__noChroot` npm derivation) with the current static aarch64
  binary (ranch-2 `d139e2e`: `@notify:` detection for user-role
  rows, agent-name resolution, all-clients `AgentNotify`
  broadcast). Verified end-to-end on the recreated VM
  (192.168.64.189): `sh` resolves inside the ranchd service PATH,
  `pi -p` under that exact PATH runs its bash tool (`PII SH OK`),
  relay joined + heartbeats flowing. The earlier rw-nix-store and
  bind-mount workarounds are dead with the old VM.
- **Phone smoke tests** — (a) H1.4 live registry behavior
  (registry screen, AgentTalk attach, avatar) on a real phone;
  (b) H6.1 locked-phone Keep-tap → belief activated;
  (c) H6.2 one-real-sample voice (mic → Parakeet → composer);
  (d) H6.3 the 5-min / 20+-turn live call; (e) H6.4 the 10×
  mid-sentence barge-in on real hardware. None need code —
  all are device-time items; `tools/herd-demo/herd-demo-h{4,6}.sh`
  print the exact steps.
- **Live voice-latency numbers: DONE 2026-10-10** — the voice services were restored on srv (RTX 4000, lab `0a0495a4`–`60dc90e7` — the Aug migration to mini CPU-ALC is reversed; mini ALC parked stopped-rollback; HA/NPM consumers back on `voice-stt/voice-tts.butler.ooo` → srv CT 201), and `.spike/voice-latency/measure.sh` ran against the LIVE Parakeet-TDT-0.6B-v3 (FP32, CUDA) + Kokoro: **TTS first chunk 283 ms (< 400 ms budget — PASS), STT 4-s partial 70 ms (< 800 ms budget — PASS)**; medians in `.spike/voice-latency/RESULTS.md`, GPU residency confirmed via nvidia-smi (3.7 GB parakeet + 450 MiB kokoro).

### Client of record (2026-10-10 directive)

The **Kotlin app** (`mobile-native/`, `dev.ranch.android`) is the
client of record for all phone acceptance above; the Expo app
(`mobile/`) was a parity reference only — it got no further feature
work and was **deleted 2026-10-16** (its open items — notification
buttons, device-recognizer parity — die with it).

### Deliberate deferrals (documented, not owed)

- **`TaskAbort` frame** (stop/abort from surfaces — Activity "stop"
  button, call stop): H5.3 part 3a shipped [open]-only items and
  H6.1 shipped the task_done Open/Fork buttons without Stop. The
  forge side exists (`POST /sessions/:id/interrupt?tree=`); only
  the frame + UI half is missing. See §10.1.
- **`ranch activity` CLI one-shot** — replaced in practice by the
  TUI Activity section + both mobile Activity UIs (§10.1).
- **Expo notification action buttons** — moot: the Expo app is deleted
  (2026-10-16); notification actions live only in the Kotlin client.
- **On-device recognizer parity beyond web** — the web hold-to-talk
  is the browser half of H6.2; there is no web equivalent of the
  on-device `SpeechRecognizer` (documented asymmetry).
- **"Secure sign-in pauses the model"** — H6.5's explicit
  out-scope remainder; the shipped substitute is the `web_login`
  broker (the model only ever sees status strings).
- **Mobile full wizard** — the phone ＋Agent form is the baseline
  form (name/profile/memory/wake); the richer half is the CLI
  `ranch agents new` wizard. Credential slots are deliberately out
  of the provisioning request (declare via `PATCH /agents/:id` +
  the web_login card).

### Durable-runtime kill-criteria reminder (H0.3)

The decision to run the agents' lives on **pi-durable upstream in a
Node harness** (no Rust reimplementation) is recorded in
`forge/docs/HERD-DURABLE-DECISION.md`. It re-opens formally if ANY
of these holds: (a) the vendor conformance suite fails on a future
pi-durable release; (b) harness↔forge IPC latency breaks the
H6.4 voice targets (stop-to-flush < 150 ms; the in-queue must stay
drained while the STT/agent/TTS hops run); (c) upstream churn
(out-of-band semantic changes, broken storage interface) makes
pinning+upgrading costlier than a reimplementation. Until then:
pinned everywhere (H0.5), thin surfaces only (`pi_agent.rs`,
`pirc/pibridge.go`).

### 2026-10-17d — call quality: thinking strip + context budget

| H6.5c (create_agent = full provisioning) | ✅ | Phone report: the executive assistant could only offer name+description when asked to create a dot (tool was the thin layer — forge's CreateAgent API always took the full row). Harness `create_agent` (forge PRs `12966da4`+`bd3e66a0`) now speaks system_prompt/description→extra_instructions, profile_id/home_machine (default = INHERIT the calling agent via GET /agents/<caller>), memory_scope/org_id/visibility/tools_allowlist, and wake_cron(HH:MM|5-field)+wake_prompt→mule POST /wakes (H3.3 forge_conversation target). MULE_URL/MULE_API_KEY env threaded config→HarnessOptions→ipc.ts installs (the boot-reinstall-only patch initially still said wake=SKIPPED — new/resumed conversations install from ipc.ts). Repo/credential binding deliberately out (approval cards). Deploy: hot-patched on the forge machine (dist→/opt/forge/harness-patched, /run systemd drop-in ExecStart, durable-pg node_modules symlink restored; reapply after image recreate: mini:~/patch-harness.sh + ~/hdist.tgz; keys persist in data-pool forge.env). E2E (Bob): 'Agent created: probe-dot-3 … wake=SCHEDULED (*/30 * * * *)' + wake verified in mule; probes deleted. vitest 69/69. |

| Live ops 2026-10-17e (phone call reliability) | ✅ multi-turn + audible agent | Real-phone report: multi-turn died mid-call, own words only appeared later, agent often never spoke back. Root causes: (1) **echo guard self-mute** — the H6.4 mic-RMS duck muted the AudioTrack when the mic was hot, but the agent's OWN speaker voice keeps the mic hot, and room noise above cold-RMS never let it back → agent inaudible (all prior E2E used voicemic.mjs, which has no echo guard — never exercised on a phone); (2) **idle out-socket death** — Cloudflare edge/cloudflared age WS out at ~100 s: the OUT socket is idle while the caller listens → dropped → every later turn silent with no captions (the “words appeared later” window — they were reaching forge's conversation, just not the socket); (3) **greeting queue** — the caller's first real turn queued behind the greeting's turnMu hold (10-min wait cap). Fixes: Kotlin (this commit, APK 10-17 10:05) echo guard only ducks when no audio played in the last 700 ms + 2.5 s watchdog, speakerphone+media focus for the call, 4×-min AudioTrack buffer, wsHttp pingInterval 25 s, out-socket re-dial (×3); mule (PR #133 `228dfb2`) 20 s out-pings + MarkOutOpen-on-attach (re-attach resumes the audio lane; new tests: re-attach, greeting cap) + greetingLockCap 45 s (abandoned greeting: Output greeting=timeout; phase-settle defer invariant documented — a stuck Speaking phase drops every in-frame = deaf call). Live-verified: NOWAIT laguna call — greeting cancel, user lane @5.1 s, answer @5.6 s; 20/20 mule pkgs green. |

| Live ops 2026-10-17d (call quality) | ✅ thinking strip + pre-call compact | “We’re not properly stripping thinking context from the agent responses in the phone call; also cap laguna at 120k context; maybe compact before the call.” Root cause (live DB): Qwen3.8 emits chain-of-thought INLINE in the projected `content` — the template auto-opens the think block, so forge’s assistant row reads `The user sent … I should respond briefly.</think>I hear you — last cookie noted.` and mule TTS’d the reasoning verbatim (forge’s `extract_assistant_text` only handles pi `thinking` BLOCKS, not inline tags — left as-is; stripping at the consumer is the right layer). Mule (PR #133 `b95e70a`): `engine.StripThinking` (paired `<think>…</think>`, dangling `<think>`→EOF, template-opened form = everything after the LAST closer; clean text untouched) applied in `latestAssistantAfter` (the fetch behind `PostAndWait`); context budget: after the model switch `ensureModel` compares session ACTIVE context (`GET /sessions/:id/context`, chars/4≈tok) vs `compact_ratio` (default .5) × target model’s `contextWindow` (`GET /v1/models/catalog` — forge `cc4f08b2` now exposes it) and submits `POST /sessions/:id/compact` when over; step config `compact`/`compact_ratio` + per-job `input_data.compact` force it; degraded-not-dead (⚠ notice, call proceeds; pi-durable generation-prep threshold = backstop). Lab ops: forge models.json laguna `contextWindow` 16000→**120000** (stale visalgo-Q4 value; real Laguna-XS trains 128k, served `-c 163840`) + forge-harness restart; note vram now serves the REAL laguna GGUF (visalgo drift 2026-09-22→10-17 documented in `hosts/vram/laguna-llama.sh`). Verified live: call → clean replies only (“2 plus 2 is 4.”); `compact:true` → “pre-call compaction submitted (requested)”, harness task completed (no-op under pi-durable’s 20k keepRecent — legitimate); auto-budget activates at next forge image rebuild (live binary predates `context_window`; absence degrades to skip). All 20 mule pkgs green. |

### 2026-10-17f — herd agent editor (mobile ✎, `ranch agents edit`)

| Live ops 2026-10-17f (agent editor) | ✅ registry + wake editing on the phone | user: “I think the mobile app still needs a herd agent editor” — correct: the app could **create** (＋Agent, H6.5) and **delete** (✕) a dot but never **change** one; renaming, re-modelling, re-scoping memory or moving an agent's home machine meant the CLI wizard (which has no edit path either) or raw `curl` against forge. Shipped the whole vertical: **protocol** — `AgentUpdate`/`AgentUpdateOk` + the wake trio `WakeList`/`WakePut`/`WakeDelete` (`WakeInfo`), and `AgentInfo` grows the editor's prefill tail (`primary_profile`, `memory_scope`, `org_id`, `visibility`, `tools_allowlist`, `extra_instructions`, `paused`) — all additive `skip_serializing_if` fields, old clients drop the unknown tags (round-trip frames added; PROTOCOL §8.6 documents the family incl. the **absent = leave the column alone** rule and the "cannot clear a nullable column" forge limitation). **daemon** — `Frame::AgentUpdate` runs the shared `agents_new::update_agent` (forge `PATCH /agents/:id`, validated before any HTTP: name ≤ 64/no newlines, `memory_scope` agent|org, `visibility` private|org, ≥ 1 field ⇒ “nothing to update”); `handle_wake` proxies mule (`GET /api/v1/wakes` filtered by `target.agent_id` OR `target.forge_conversation.agent_id` OR `spec.agent_id` — the three shapes a wake can carry, so the editor never hides a wake the agent will hear; `POST`/`PATCH`/`DELETE /api/v1/wakes[/{id}]`). A **blank cron on an update means "leave the schedule alone"** — how pause/resume works and how non-cron kinds (no `spec.expr`) stay editable; a create still requires one. New `reply_to` helper for single-client replies. **forge worker** — `AgentList` now decodes through the one `agents_new::agent_info_from_value` used by the PATCH reply and the CLI, so list/edit can't drift. **CLI parity** — `ranch agents edit <name|id>` (keeps current values as `[bracket]` defaults; `*` = allow all tools), `agents_new::run(&[String])`; usage text updated. **mobile** — `EditAgentActivity` (✎ on each registry row, whole row rides along as the prefill): name/avatar/profile chip/home machine/memory scope + org/visibility/tools checkboxes/extra instructions, Save sends ONLY the diff; a **wakes** section (`WakeList`) with per-row edit/pause/resume/delete + ＋ add wake saving immediately; profile picker + inline new-profile form + tools set extracted to **`AgentForm.kt`**, now shared by the builder and the editor (the builder was refactored onto it — one implementation of "what the ＋Agent form writes" is exactly what the editor shows). Gates: `cargo test -p ranch -q` **85** (+6: patch-body/prefill-decode/wake-filter/wake-patch-body/schedule-rule + protocol round-trip), `cargo test -p ranch-protocol -q` 14, clippy 0 (only the pre-existing `voicecall.rs` collapsible_if), `cargo fmt --all` clean (also normalized three pre-existing mis-indented `background: None` lines in `mule.rs`/`pilocal.rs`), `./gradlew assembleRelease` BUILD SUCCESSFUL (APK 1.39 MB). **e2e against a mock forge+mule** (isolated `HOME` + throwaway daemon): `AgentUpdateOk` returns the patched row with all editor fields; `WakeListOk` filters to the agent; `WakePut` update + create; `WakeDeleteOk`; `Error` paths `nothing to update` / `memory_scope must be 'agent' or 'org'`; CLI `agents edit` PATCH body verified field-by-field. **Owed**: on-device smoke of the ✎ screen (no phone session here), Expo/web form parity (the ＋Agent builder is still native-only too), and credential-slot editing (`credentials_scope`) stays out — declared via forge + the `web_login` card as in H6.5. |
