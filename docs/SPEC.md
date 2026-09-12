# Ranch — System Specification

Version: 1.0 (target system). Status: the architecture described here is
the product we are building; features not yet shipped are marked
**- todo** in the README and carry a milestone in docs/PLAN.md.

Companion documents:

- [PLAN.md](PLAN.md) — implementation plan, grounded in the codebase.
- [PROTOCOL.md](PROTOCOL.md) — wire protocol reference.
- [design/](design/) — design docs + diagrams (agent tool surface,
  webhook relay, workflow integration).
- [history/](history/) — archived spec versions + milestone completion
  notes (the build log).

---

## 1. What Ranch is

Ranch is a **workbench for humans and agents, built on a terminal
multiplexer**. It owns long-lived sessions on your machine — shell
panes, agent panes, workflow panes — reachable from your terminal, any
browser, or your phone. A session keeps running while you're away;
reconnect from any device and pick up exactly where you left off.

Four tools, one product:

| tool   | role in the system | repo |
|---|---|---|
| **Ranch** | the surface: terminal multiplexer, TUI/web/mobile clients, the daemon that owns everything | mule-ai/ranch |
| **Forge** | durable agent runtime: long-lived pi-backed agent sessions, persisted conversation, durable resume | mule-ai/forge |
| **Pi**    | local agent runtime: `pi --mode rpc` as a child of the daemon; zero-infra agent panes | badlogic/pi-mono |
| **Mule**  | workflow orchestration: multi-step agent workflows, jobs, triggers | mule-ai/mule |

### Tenets

1. **Ranch is a terminal multiplexer.** Shell panes are first-class
   citizens — tmux-grade splits, windows, scrollback, keybindings. Every
   agent feature is additive to this core; nothing degrades the shell
   experience.
2. **Ranch is for humans.** Viewing and editing files on the machine is
   built in (file browser, editor, markdown review) — on every surface.
3. **Ranch enables humans to build agents.** Creating and configuring a
   forge agent (profile, model, skills, system prompt, working
   directory) is done *in ranch*, with the same UX on every surface.
4. **Ranch enables agent workflows.** Mule workflows are first-class
   panes: CRUD + run from any client, plus cron and event-based
   triggers.
5. **Ranch is for agents.** Agents running in ranch panes get tools to
   spawn, steer, and reap sub-agent panes — orchestration a human can
   *see*, because every sub-agent is a real pane in the layout.

The pitch: *your ranch, your machines, your agents, your workflows.*

---

## 2. Architecture

```
                                   ┌───────────────────────────────┐
                                   │  Supabase (cloud)             │
      ┌────────────┐   frames      │  • Auth (JWT, OAuth)          │
      │ TUI client ├──────────────▶│  • Postgres: machines,        │
      │ (CLI)      │               │    sessions mirror, triggers  │
      └────────────┘               │    (mirror), webhook log      │
      ┌────────────┐               │  • Realtime: private channel  │
      │ Web client ├──────────────▶│    per machine                │
      │ (browser)  │               │  • Edge function: webhook     │
      └────────────┘               │    receiver → relay           │
      ┌────────────┐               └──────▲──────────────┬─────────┘
      │ Mobile app ├───────────────────────┘              │ webhooks
      │ (Android)  │                                      │ (external
      └────────────┘                                     │  services)
                                                         │
┌────────────────────────────────────────────────────────▼─────────┐
│ ranchd (on your machine)                                          │
│                                                                   │
│  ┌───────────┐  ┌──────────────────────────────────────────────┐  │
│  │ registry  │  │ session "work"                               │  │
│  │ sessions  │  │ ┌──────────┐ ┌──────────┐ ┌───────────────┐  │  │
│  │ windows   │  │ │ shell    │ │ shell    │ │ agent (forge) │  │  │
│  │ panes     │  │ │ PTY      │ │ PTY      │ │ chat pane     │  │  │
│  │ clients   │  │ │ ghostty  │ │ ghostty  │ │ + sub-panes ──┼──┤  │
│  └───────────┘  │ │ -vt      │ │ -vt      │ │  (agent-      │  │  │
│                 │ └──────────┘ └──────────┘ │   spawned)    │  │  │
│  ┌───────────┐  └──────────────────────────────────────────────┘  │
│  │ workers   │  ┌──────────────────────────────────────────────┐  │
│  │ • forge   │  │ session "ops"                                │  │
│  │ • pi rpc  │  │ ┌──────────────────────┐ ┌────────────────┐  │  │
│  │ • mule    │  │ │ workflow pane        │ │ agent (pi)     │  │  │
│  │ • webhooks│  │ │ (mule run stream)    │ │ chat pane      │  │  │
│  │ • trigger │  │ └──────────────────────┘ └────────────────┘  │  │
│  │   scheduler│ └──────────────────────────────────────────────┘  │
│  └───────────┘                                                   │
│  unix socket (local clients)   relay WS (cloud clients)          │
└────────┬──────────────────────────────────────┬───────────────────┘
         │                                      │
   ┌─────▼─────┐                         ┌──────▼──────┐
   │ forge-api │                         │ mule (Go)   │
   │ (durable  │                         │ workflows,  │
   │  agents)  │                         │ jobs, WS hub│
   └───────────┘                         └─────────────┘
```

### 2.1 Core model

- **machine** — one `ranchd` installation; registered with a Supabase
  account; identified by a machine key (SHA-256 stored server-side).
- **session** — a named stack of **windows**; the durable unit of work.
- **window** — a binary split tree of panes (tmux semantics).
- **pane** — one unit of content. Kinds:
  - `pty` — a shell (or any program) on a PTY + a `libghostty-vt`
    terminal emulated **server-side** (the daemon owns the VT; clients
    render the grid and send keys).
  - `chat` — an agent conversation (forge-backed or local-pi-backed).
    No PTY; rows come from forge's messages table or pi RPC events,
    rendered as chat bubbles on all clients.
  - `workflow` — a mule workflow run streamed into a pane.
- **client** — anything attached: local TUI, web app, mobile app, or an
  agent tool session. Many clients per pane; identical state.

### 2.2 The daemon (`ranchd`)

One process per machine (systemd user unit; `ranch daemon` /
`ranchd` argv0 / `--daemon`). Responsibilities:

- **PTY manager** — spawn/kill pane processes; resize via
  `TIOCSWINSZ` + SIGWINCH.
- **VT owner** — one `libghostty-vt` per pane; 30 ms coalescing tick;
  dirty-row diffs → `update` frames; per-pane `seq`; resync on gaps.
- **Registry** — sessions/windows/panes in `state.json` (Tier-1 cold
  restore) + `ranch upgrade` hot-upgrade via re-exec with fd
  inheritance (Tier-2, zero pane death).
- **Local server** — unix socket, JSON-lines, same frame schema as the
  relay.
- **Relay client** — persistent WS to the Supabase Realtime private
  channel; "just another client" via pipe fds; heartbeat + session
  mirror.
- **Workers** —
  - *forge worker*: one SSE stream per watched forge session;
    `ChatSend` → `POST /messages`; model catalog/switch.
  - *pi harness*: local `pi --mode rpc` children; RPC events → chat
    rows; hot-upgrade-safe (pipes fd-inherited).
  - *mule worker*: workflow CRUD proxy + run streaming (mule WS hub
    events → workflow pane rows).
  - *trigger scheduler* **- todo**: cron + event evaluation (§6).
  - *webhook listener* **- todo**: authenticated inbound events (§7).

### 2.3 Clients

All three surfaces speak the same frame protocol and implement feature
parity for every user-facing capability:

| capability | TUI (ratatui) | Web (React) | Mobile (Expo/Android) |
|---|---|---|---|
| session list / dashboard | ✅ | ✅ | ✅ |
| attach: grid render + keys | ✅ | ✅ | ✅ |
| splits / windows / focus / resize | ✅ | ✅ | ✅ (keys row) |
| scrollback | ✅ | ✅ | ✅ |
| agent chat panes (forge + pi) | ✅ bubbles | ✅ bubbles | ✅ bubbles |
| agent model picker | ✅ | ✅ | ✅ |
| file browser + editor + md review | ✅ | ✅ | ✅ |
| forge agent builder (profiles) | ✅ **- todo** | ✅ **- todo** | ✅ **- todo** |
| mule workflow CRUD + run | ✅ **- todo** | ✅ **- todo** | ✅ **- todo** |
| workflow run panes | ✅ **- todo** | ✅ **- todo** | ✅ **- todo** |
| triggers (cron/event) management | ✅ **- todo** | ✅ **- todo** | ✅ **- todo** |
| agent spawn/steer/close tools | daemon-side | daemon-side | daemon-side |
| webhook receiver config | ✅ **- todo** | ✅ **- todo** | ✅ **- todo** |

The web client is the reference thin client (browser = phone = TUI).
Local TUI additionally owns tmux-style keyboard bindings; mobile adds
touch affordances (tap-to-expand tool rows, long-press kill).

---

## 3. Shell panes (multiplexing core)

Unchanged from v0.1 and still the foundation:

- Sessions survive disconnects; daemon restarts restore from
  `state.json` (Tier 1); `ranch upgrade` re-execs with every
  PTY/agent/listener fd inherited — children never notice (Tier 2).
- tmux-flavored keys: `Ctrl-B` prefix — `%`/`"` split, arrows focus,
  Ctrl-arrows resize, `c`/`n`/`p`/`0-9` windows, `&` window kill, `x`
  pane kill, `{`/`}` swap, `s` sidebar, `,` rename, `:` prompt, `d`
  detach. `Ctrl-B Ctrl-B` passes a literal through.
- Server-side emulation: exact reconnect from any device; droppable
  coalesced updates; chunked snapshots under the Realtime cap.
- The demo sandbox shell (`SHELL=qjs`) remains the zero-trust default
  for public machines.

---

## 4. Files: viewing and editing

The daemon is the file server; clients are thin editors (same
philosophy as terminal emulation — server owns state, clients render).

Shipped:

- `DirList` / `FileRead` / `FileWrite` frames; atomic writes (temp +
  rename); mtime conflict detection (no silent clobber); daemon-side
  watch set with mtime polling → `FileChanged` push; dirty-editor
  banner (reload / keep-mine) on mobile.
- Mobile: directory browser, monospace editor (JetBrains Mono),
  markdown review via `marked` token rendering.
- Web: editor surfaces ride the same frames.

**- todo** — TUI file browser/editor over the same frames (rofi-style
picker + `$EDITOR` handoff for full editing; inline editing for quick
fixes).

---

## 5. Agents: forge + pi

### 5.1 Agent panes (shipped)

- `kind:"forge"` chat pane — bound to a forge session; forge worker
  streams SSE (`GET /sessions/{id}/events?since=`), live delivery
  ~10-15 ms; history replays on attach; conversation durable in forge's
  Postgres.
- `kind:"pi"` chat pane — local `pi --mode rpc` child; identical chat
  UX; session file recorded in `state.json` for restore; pipes survive
  hot upgrade.
- Anchor to a directory: `prefix-a` anchors the new agent pane to the
  focused pane's cwd; `ranch agent <name> [dir]` anchors to the
  invoking shell's cwd.
- Model picker: `ModelList` / `ModelSet` frames (pi `set_model`; forge
  session overrides). Working/idle metas power the typing indicator on
  all clients.

### 5.2 Agent configuration (agent builder) **- todo**

Goal: build and configure a forge agent without leaving ranch — on any
surface. Ranch becomes the configuration front-end for forge.

Forge gains (see docs/PLAN.md Phase B1 for the forge work items):

- **Profile CRUD hardening** — forge has `POST/GET /profiles`,
  `PATCH /profiles/update`, `DELETE`; ranch surfaces it as a form:
  name, description, provider + model (from `GET /v1/models/catalog`),
  system prompt, tools allowlist, working dir, git url/ref, nix shell.
  API key entry is write-only (forge redacts on read).
- **Skill management** — forge profiles reference pi skills; ranch
  provides a skills browser (list from forge, preview SKILL.md,
  attach/detach to a profile).
- **Per-session overrides** — model override already works
  (`override_provider`/`override_model`); ranch exposes it per chat
  pane (shipped: ModelSet) and per profile (builder).
- **Agent recipes** — named, versionable templates (profile + skill
  set + suggested workflow steps) that ranch renders as a
  "new agent" wizard. Recipes live in forge; ranch is the editor.

Wire surface: new proxy frames `ProfileList/ProfileGet/ProfilePut/
ProfileDelete` and `SkillList/SkillGet` in ranch-protocol, proxied by
the forge worker (same worker/pipe pattern as ForgeList today). No
forge API key ever reaches a client.

### 5.3 Agents for agents: the ranch tool surface **- todo**

Agents running in ranch (forge or pi panes) get **ranch tools** so they
can orchestrate visibly:

- `ranch_spawn` — create a session (or split a new pane off the
  caller's session) running an agent (forge profile or local pi) with
  an initial prompt. Returns the pane/session id. The new pane is a
  real pane: the human sees it appear, can focus it, read it, type
  into it, or close it.
- `ranch_send` — steer a spawned pane: send a message to its agent
  (forge `POST /messages` / pi `steer`/`follow_up` RPC), with
  delivery mode `steer` (interrupt-friendly) or `queue`.
- `ranch_status` — read a spawned pane's state (working/idle, last
  rows, exit status) without attaching.
- `ranch_read` — read a spawned pane's recent conversation rows
  (chat rows or terminal tail), so the caller can consume results.
- `ranch_close` — close a spawned pane when its work is done (kills
  the pi child / unbinds the forge watch; sessions spawned by the
  agent die with their last pane). Agents can only close panes they
  spawned (ownership recorded daemon-side).
- **Callbacks** — every `ranch_spawn` registers a completion callback:
  when the spawned agent's turn ends (`turn_ended` SSE event / pi
  `agent_end` RPC event), the daemon delivers a `tool_result`-
  equivalent completion notification — as a chat row injected into the
  caller's pane (`⟨pane name⟩ finished: <last assistant row>`) plus a
  resolvable waiter for the agent harness (forge: a pending message
  the harness returns to the caller's pi; pi-local: an RPC event).
  The caller never polls.

Design details in [design/agent-tools.md](design/agent-tools.md).

Security posture: agent tools are daemon-authoritative — every tool
executes in the daemon (`agenttools.rs`), regardless of where the
agent runs. Ownership (who spawned which pane) is recorded
daemon-side; `ranch_close` is spawn-scoped. Optional `ask` policy
surfaces an approval chip in every attached client. Because the agent
runtime is pi **or** forge — and forge usually runs on a different
host — there are two transports, both daemon-initiated (the daemon
has no inbound listener): local-pi agents use a loopback control API
(§6.1 of the design doc); forge agents go through a forge-hosted
bridge (the agent's tools forward to forge, forge queues + streams
the request to the daemon on the SSE stream it already consumes, the
daemon executes and POSTs the result back). Forge implements the same
bridge for its own agents; ranch is the policy point either way.

---

## 6. Workflows: mule

### 6.1 Workflow panes and CRUD **- todo**

The mule worker (daemon) proxies mule's REST + WS surfaces into ranch
frames; clients never talk to mule directly (no mule credentials on
devices):

- **CRUD** — `WorkflowList/ListOk`, `WorkflowGet/GetOk`,
  `WorkflowPut/PutOk` (create + update: name, description, is_async,
  steps incl. reorder), `WorkflowDelete/DeleteOk`, proxied to mule's
  `/api/v1/workflows`, `/api/v1/workflows/{id}/steps`. Same for the
  ingredients: `AgentList/*` (`/api/v1/agents`), `SkillList/*`
  (`/api/v1/skills`), `ProviderList/*` (`/api/v1/providers`).
  Clients render a workflow editor: step list, per-step type (agent /
  wasm), agent picker, config JSON with schema hints.
- **Run** — `WorkflowRun {workflow, input?}` → mule `POST /api/v1/jobs`
  → a **workflow pane** (`kind:"workflow"`) tees the run: mule WS hub
  events (`job_update`, `job_step_update`, agent events: `message_update`,
  `tool_execution`) are formatted to rows and streamed into the pane's
  VT (plain text, scrollable) — watchable live from any client.
- **Status** — `meta {kind:"workflow", status:"running"|"completed"|
  "failed", job_id}` per workflow pane; dashboard badges.

### 6.2 Triggers **- todo**

Workflows run without a human present. Ranch (daemon-side trigger
scheduler) supports three trigger types, managed as first-class
resources:

- **cron** — standard cron expression + timezone; daemon schedules
  locally (runs even when no client is attached; survives restart via
  `state.json`; hot-upgrade-safe).
- **event** — a predicate over ranch/mule events: *workflow completed*,
  *agent turn ended* (forge `turn_ended` / pi `agent_end`), *file
  changed* (the §4 watch set), *pane exited*. Events from forge/mule
  arrive on the workers' existing streams; file events from the
  watcher. Predicates are simple JSON (event type + id match +
  optional payload filter).
- **webhook** — external services POST to the relay receiver (§7);
  a webhook trigger matches on source + payload shape and can template
  workflow input from the payload.

Trigger resource:

```jsonc
{
  "id": "trg_…",
  "name": "nightly-review",
  "workflow": "wf_…",
  "type": "cron",                  // cron | event | webhook
  "schedule": { "cron": "0 3 * * *", "tz": "America/Los_Angeles" },
  "input": { "branch": "main" },   // templated job input
  "enabled": true,
  "last_run": { "job": "job_…", "at": "…", "status": "completed" }
}
```

Frames: `TriggerList/*`, `TriggerPut/*`, `TriggerDelete/*`,
`TriggerRun` (manual fire). The trigger registry mirrors to Supabase
so the dashboard can show upcoming/last runs for offline machines.

---

## 7. Webhook receiver (relay) **- todo**

External systems (GitHub, CI, mule itself, forge) need to start work
on a machine. Supabase Edge Function `webhook-relay` (deployed in the
ranch Supabase project) is the receiver; it validates, routes, and
forwards into the per-machine Realtime channel the daemon is already
listening on.

### 7.1 Addressing & auth

- **URL**: `https://<project>.supabase.co/functions/v1/webhook-relay/
  w/<webhook_id>` — one webhook id per (machine, purpose); ids are
  UUIDs, unguessable.
- **Auth**: HMAC-SHA256 signature over the raw body in
  `X-Ranch-Signature: t=<unix>,v1=<hex>`, keyed by a per-webhook
  secret (shown once at creation, stored hashed in `webhooks`). The
  function rejects stale timestamps (±5 min clock skew) and bad
  signatures (constant-time compare). Optionally also verify GitHub's
  `X-Hub-Signature-256` scheme for GitHub sources (same table).
- **Routing**: the function looks up `webhook_id` → machine id +
  allowed event types; wraps the body in a `WebhookEvent` frame;
  broadcasts on the machine's private Realtime channel **using the
  service role** (edge-function-only key; never shipped to clients).
- **Delivery**: the daemon receives `WebhookEvent` frames like any
  other client frame (the relay pipe), matches them against webhook
  triggers (§6.2), and runs workflows. Events also land in a
  `webhook_log` table (bounded, e.g. 7 days) for debugging — the
  function logs the accepted envelope, never raw bodies by default
  (bodies only when `debug` is set on the webhook row).

```jsonc
// WebhookEvent frame (relay → daemon)
{
  "t": "WebhookEvent",
  "webhook": "trg_…",             // webhook id
  "source": "github",             // free-form source tag
  "event": "push",
  "payload": { … },               // verified body (JSON), capped 64 KB
  "received_at": "…"
}
```

- **Multi-user**: each machine's webhook ids live under that
  machine's owner (RLS on `webhooks`); the URL path (webhook id) plus
  HMAC makes cross-tenant delivery impossible without the secret.
- **Rate limiting**: per-webhook token bucket in the function
  (configurable; default 60/min) — abusive sources get 429 and a log
  row, not a workflow storm.

Schema (Supabase migration):

```sql
create table webhooks (
  id         uuid primary key default gen_random_uuid(),
  user_id    uuid not null references auth.users on delete cascade,
  machine_id uuid not null references machines on delete cascade,
  name       text not null,
  secret_hash text not null,        -- sha256; raw shown once
  sources    text[] not null default '{}',  -- allowed source tags
  debug      boolean not null default false,
  created_at timestamptz not null default now()
);
create table webhook_log (
  id         bigint generated always as identity primary key,
  webhook_id uuid not null references webhooks on delete cascade,
  source     text, event text, status int,
  received_at timestamptz not null default now()
);
alter table webhooks      enable row level security;
alter table webhook_log   enable row level security;
-- owner-only RLS policies (same pattern as machines)
```

### 7.2 Non-goals

- The receiver never executes workflow logic — it validates and
  forwards. All policy lives daemon-side.
- No inbound *terminal* access via webhooks — the receiver can only
  emit `WebhookEvent` frames; it cannot drive panes.

---

## 8. Wire protocol additions (summary)

New frames (full reference lives in PROTOCOL.md as they land):

| family | frames |
|---|---|
| profiles (forge) | `ProfileList/ProfileListOk`, `ProfileGet/ProfileGetOk`, `ProfilePut/ProfilePutOk`, `ProfileDelete/ProfileDeleteOk` |
| skills (forge) | `SkillList/SkillListOk`, `SkillGet/SkillGetOk` |
| workflows (mule) | `WorkflowList/WorkflowListOk`, `WorkflowGet/WorkflowGetOk`, `WorkflowPut/WorkflowPutOk`, `WorkflowDelete/WorkflowDeleteOk`, `WorkflowRun/WorkflowRunOk` |
| triggers | `TriggerList/TriggerListOk`, `TriggerPut/TriggerPutOk`, `TriggerDelete/TriggerDeleteOk`, `TriggerRun/TriggerRunOk` |
| webhooks | `WebhookList/…`, `WebhookPut/…`, `WebhookDelete/…`, `WebhookEvent` (relay → daemon) |
| agent tools | `AgentSpawn/AgentSpawnOk`, `AgentSend`, `AgentStatus/AgentStatusOk`, `AgentRead/AgentReadOk`, `AgentClose/AgentCloseOk`, `AgentDone` (daemon → caller pane, completion callback) |

All follow the existing conventions: `req_id` correlation, broadcast +
client-side match, `Error {req_id, message}` on failure, unknown
types ignored (extensibility rule), chunking for oversized payloads.

---

## 9. Data model (Supabase Postgres)

Existing: `machines`, `sessions` (mirror), `realtime.messages` RLS.

Added by this spec **- todo**:

- `triggers` (mirror of daemon state; RLS owner-only) — see §6.2 for
  shape; `machine_id` FK.
- `webhooks`, `webhook_log` — see §7.

The daemon remains the source of truth for triggers; the mirror exists
so dashboards can render trigger state for offline machines and so the
webhook edge function can resolve routing without daemon round-trips.

---

## 10. Security model

Unchanged foundations:

- Local: unix socket 0600, same-uid only.
- Machine key: shown once at `ranch register`; only SHA-256 stored;
  Realtime channels RLS-gated (owner + machine).
- Terminal content transits Supabase in TLS but is readable by the
  relay in transit (documented accepted risk; E2E designed-for-later).
- Forge/mule API keys live only in `daemon.toml` (0600) on the daemon
  host; never sent to clients.

New in this spec:

- **Agent tools are daemon-authoritative.** Agents get tools through
  their harness (forge tool executor / pi extension), each mapped to a
  daemon control call; ownership (who spawned which pane) is recorded
  daemon-side; `ranch_close` is spawn-scoped. Optional `ask` policy
  routes every spawn to human approval chips on all attached clients.
- **Webhooks**: per-webhook HMAC secret, timestamp-fresh signatures,
  constant-time comparison, service-role isolation (the edge function
  can only broadcast, never read terminal frames), 64 KB payload cap,
  per-webhook rate limits, bounded `webhook_log`.
- **Profiles/secrets**: profile API keys are write-only through ranch
  (forge redacts on read); ranch clients never see them.

---

## 11. Repository layout & build

```
ranch/
├── crates/
│   ├── ranch-protocol/   frame types, framing, chunking (no I/O)
│   ├── ranch-vt/         libghostty-vt FFI wrapper (server-side VT)
│   └── ranch/            the single binary: client (client.rs) +
│                         daemon (daemon.rs, forge.rs, pilocal.rs,
│                         relay.rs, mule.rs*, triggers.rs*,
│                         webhooks.rs*, agenttools.rs*)
├── web/                  Vite + React web app (GitHub Pages)
├── mobile/               Expo/Android app
├── supabase/
│   ├── migrations/       schema + RLS (+ webhooks, triggers, log)
│   └── functions/        edge functions (webhook-relay) *- todo
├── systemd/ranchd.service
├── docs/                 SPEC, PLAN, PROTOCOL, design/, history/
├── vendor/ghostty/       pinned ghostty source (gitignored)
└── Makefile              build/test/install/service
```

`*` = new modules planned by this spec (see PLAN.md).

Build story unchanged: single static binary (musl + libghostty-vt
static archive, Zig 0.16 pin), `make build` / `make install` /
`make service`; `ranch upgrade` for zero-downtime deploys.

---

## 12. Risks & open questions

1. **Mule WS hub is a global firehose** (`BroadcastJobUpdate` fans out
   to all clients; no per-job subscription). Mitigation: the mule
   worker filters server-side (by job id before frames cross the
   relay); propose upstream per-job subscription channels.
2. **Forge bridge plumbing** — remote forge agents calling ranch
   tools depend on forge hosting the request queue (F3a) and the
   callback notify hook (F3b). Both are additive and mirror existing
   patterns (`/tools/execute`, bus publish). Design in
   design/agent-tools.md §6.2; risk contained — worst case, callers
   poll `ranch_status` and callbacks are the optimization. Ordering
   note: A1/A2 (daemon-side registry + frames) and A3-local (loopback
   + pi extension) ship before F3a; forge-bridge tool calls are a
   follow-on, not a blocker.
3. **Supabase edge function + service role** — the webhook receiver
   holding service-role credentials widens the trust surface; accepted
   (it can only insert to Realtime channels we route, payload-capped),
   but the function must be minimal and audited.
4. **Realtime limits** (28 KB chunks, throughput) — existing chunking +
   coalescing + resync apply to all new frame families.
5. **Trigger clock authority** — cron schedules run on the daemon
   (machine local time / configured tz); mirrors are eventually
   consistent. A machine that's offline misses cron runs (documented;
   catch-up-on-reconnect optional per trigger).
6. **Mixed-client canonical size** — most-recent-active-client rule
   stands; revisit per-client virtual sizes if phone-first workflows
   demand it.
