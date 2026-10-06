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
   (Expo `mobile/`), and mobile-native (`mobile-native/`) in the same
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
- Web: Expo web build of `mobile/` (same React Native app).
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
| H4 | Memory & learning: episodes/beliefs (pgvector), episode capture, prompt sections, reflection workflows, belief-approval cards, cross-agent signals | forge, mule, ranch |
| H5 | Proactivity: read-only research tasks, wake conditions (incl. memory triggers), Activity View, runaway guardrails | forge, mule, ranch |
| H6 | Presence & voice: mobile agent notifications+actions, voice input, voice loop v1 + barge-in v2, specialist provisioning UX | ranch (TUI/web/native), mule, forge |

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
  request/attach path; the push is a new `Meta { kind:"activity",
  items }` broadcast (throttled: only on item set change, debounced
  2 s) so clients update a timeline without polling.
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
  mule `PATCH /api/v1/wakes (enabled=false) for agent`. Per-agent +
  global variants.
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
  the same policy (the Expo app's `mobile/lib` notification code is
  the reference; keep the *policy* in one shared module).
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
(`mobile/`), mobile-native (`Monitor.kt`), and the raw test client
(`tools/relay-test.mjs` updated to print them).

| Frame | Phase | Direction | Purpose |
|---|---|---|---|
| `AgentList` / `AgentListOk` | H1 | c↔d | agent registry |
| `AgentTalk` / `AgentTalkOk` | H1 | c→d / d→c | open/continue/fork a conversation |
| `AgentStatus` / `AgentStatusOk` | H1 | c↔d | busy + active conversation |
| `AgentActivity` / `AgentActivityOk` | H5 | c↔d | activity pull |
| `Meta { kind:"activity", … }` | H5 | d→c | activity push (debounced) |
| `TaskAbort { task?, agent?, all? }` | H2/H6 | c→d | tree abort (Activity "stop", call "stop") |
| `AgentAskRequest { card?: {kind, agent, …} }` (extension of L761) | H3/H4/H5 | d→c | approval/memory/research cards |
| `VoiceCall { req_id, agent, in_url?, out_url? }` / `VoiceCallOk` | H6 | c↔d | call transport session (mule job id) |
| `AgentPause { agent?, all? }` / `AgentPauseOk` | H5 | c→d | kill switch |

Ranch CLI additions (`main.rs` dispatch + `client.rs`):
`ranch agent [name] [msg…]` (H1), `ranch call <agent>` (H6.3),
`ranch activity` (H5, prints the timeline once),
`ranch agents new` wizard (H6.5).

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
| H0 | ◐ in progress | H0.1 ✅ (vendor @ ae92585d = pi-durable 1.0.4, digest notes written), H0.2 ⏳ (durable-pg in progress — subagent flakiness; being driven directly), H0.3 ✅ (decision record at `forge/docs/HERD-DURABLE-DECISION.md`), H0.4 ◐ config written (`lab/ct/forge` pgvector ✅, `lab/ct/mule` machine config ✅ @ port 8091) — **pending human: image build + `lab-alc-reconcile` on mini + secrets + consumer repoint**, H0.5 ✅ code-side (extension pins + ranchd drift warning; image rebuilds ride H0.4's reconcile) |
| H1 | ☐ not started | |
| H2 | ☐ not started | |
| H3 | ☐ not started | |
| H4 | ☐ not started | |
| H5 | ☐ not started | |
| H6 | ☐ not started | |
