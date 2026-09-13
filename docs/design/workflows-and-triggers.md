# Design: Workflow Panes, Triggers, and the Mule Integration

*How mule workflows become first-class ranch objects: CRUD from any
client, runs streamed into panes, and triggers (cron / event /
webhook) that run them without a human.*

Status: design (Phases C–E of PLAN.md).

## 1. Integration shape

```
┌──────────────── ranchd ─────────────────┐
│                                         │
│  mule.rs worker (own pipe pair)         │
│   ├─ REST: /api/v1/workflows CRUD       │
│   │        /api/v1/agents|skills|providers (pickers)
│   │        POST /api/v1/jobs (run)      │
│   └─ WS:   /ws hub → job_update,        │
│            job_step_update, agent events│
│            (filter by job_id, format    │
│             rows → pane VT)             │
│                                         │
│  triggers.rs                            │
│   ├─ cron ticks (daemon poll loop)      │
│   ├─ event hooks (internal emit points) │
│   └─ WebhookEvent matches (webhooks.rs) │
│         │                               │
│         ▼ fire                          │
│   POST /api/v1/jobs ──▶ workflow pane   │
└─────────────────────────────────────────┘
          │ HTTP + WS
   ┌──────▼──────┐
   │ mule (Go)   │
   │ :8080       │
   └─────────────┘
```

A **workflow pane** is a headless pane: a `libghostty-vt` with **no
PTY** — the mule worker writes formatted run output into the VT; from
there it's ordinary ranch plumbing (dirty-row diffs, snapshots,
scrollback, relay). The human can attach from any device, split it
next to their shell, and scroll its history.

## 2. Workflow object mapping

| mule | ranch |
|---|---|
| `Workflow {id, name, description, is_async}` (`internal/primitive/primitive.go:54`) | workflow list rows + editor |
| `WorkflowStep {step_order, type, agent_id, wasm_module_id, config}` (`:88`) | step editor (ordered list; reorder via `/steps/reorder`) |
| `Agent {name, provider_id, model_id, system_prompt, pi_config}` (`:30`) | step config picker (MuleAgentList frames) |
| `Job {status: QUEUED→RUNNING→COMPLETED\|FAILED}` (`pkg/database/models.go:86`) | pane meta + dashboard badges |
| `JobStep {status, input_data, output_data}` | per-step timeline rows in the pane |

The run stream: mule's WS hub broadcasts `WebSocketMessage{type, data,
timestamp}` to **all** clients (`websocket.go:120`) — a global
firehose with no per-job subscription, plus a `JobStreamer` that polls
the job store every 2 s and broadcasts status changes
(`websocket.go:181-211`). The worker: connects once, filters by
`job.id` from `job_update`/`job_step_update` payloads, and formats:
step headers (`▶ step 2/4: "review" (agent pr-reviewer)`), agent text
deltas, tool executions (`⚙ bash · 1.2s`), step results. Job failure
prints `output_data` tail. Status latency is bounded by the 2 s
streamer poll — fine for panes; don't add a second polling path.

Alternative run path (kept in mind, not used for v1): mule also
executes via `POST /v1/chat/completions` with
`model: "agent/<name>" | "workflow/<name>" | "async/workflow/<name>"`
(`handlers.go:162-258`; note agents are looked up by **name**, and the
async form returns `{id, object:"async.job", status}` — the same job
id the WS stream keys on). Ranch uses `/api/v1/jobs` (id-addressed,
takes `input_data` + `working_directory`) because names are mutable;
the chat-completions path stays useful for one-shot agent calls from
trigger templates later.

Follow-up upstream (file a mule issue): per-job subscription channels;
until then the worker filter is server-side (never crosses the relay).

## 3. Workflow editor (client surfaces)

- **Web** (reference implementation): Workflows page — list with
  run/edit/delete; editor: name/description/async, step cards
  (type: agent|wasm; agent dropdown; config textarea with JSON
  validation; drag-order later, `↑/↓` buttons first), save →
  `WorkflowPut` (worker composes mule's `PUT /workflows/{id}` +
  step create/update/delete/reorder calls — the ranch draft is the
  desired end-state, the worker diffs).
- **TUI**: `:workflows` overlay (sidebar pattern, M4.2): list,
  `Enter` run, `e` edit → step list with JSON config editing (same
  simple editing as `:files`), `n` new.
- **Mobile**: list + run; editor is read/view + run on v1 (form
  editing on a phone is a web/TUI task); steps shown as a timeline.

## 4. Triggers

### 4.1 Registry

```jsonc
{
  "id": "trg_…", "name": "nightly-review",
  "machine": "<uuid>",             // multi-machine: which daemon runs it
  "workflow": "wf_…",
  "type": "cron|event|webhook",
  "cron":   { "expr": "0 3 * * *", "tz": "America/Los_Angeles", "catch_up": false },
  "event":  { "kind": "agent_turn_ended",            // agent_turn_ended | workflow_completed
            | "workflow_completed" | "file_changed" | "pane_exited" | "webhook",
              "filter": { "workflow": "wf_…", "pane": "…", "path_glob": "…",
                          "webhook_source": "github", "payload_glob": "…" } },
  "input": { "branch": "main" },   // job input; supports $ substitutions:
                                   //   {event.payload.x}, {last_run.output.y}
  "enabled": true,
  "last_run": { "job": "job_…", "at": "…", "status": "completed" }
}
```

Truth lives daemon-side (in `state.json`); a mirror row in Supabase
(`triggers` table) serves dashboards for offline machines. `TriggerPut`
from a client always lands on the daemon (via relay if remote); the
mirror is written by the daemon.

### 4.2 Event sources (all internal, zero new streams)

| event | existing emit point |
|---|---|
| `agent_turn_ended` | forge worker idle metas (`forge.rs` turn_ended handling) + pi RPC `turn_end` (`pilocal.rs`) |
| `workflow_completed` | mule worker `job_update` terminal status |
| `file_changed` | the M10 phase-3 file watch loop (daemon tick, mtime poll) |
| `pane_exited` | dead-pane cleanup in the poll loop |
| `webhook` | `WebhookEvent` frames (design/webhook-receiver.md) |

Evaluation: triggers.rs subscribes to an in-daemon event bus (a small
`tokio::sync::broadcast` or the existing meta broadcast path); filters
match; fire = mule `POST /api/v1/jobs` with templated input +
`last_run` update + `TriggerFired` broadcast.

### 4.3 Cron

- Parse with the `cron` crate (schedule expressions); next-fire
  computed per trigger; the daemon's existing poll loop checks due
  triggers at 1 s resolution.
- Timezone: `tz` via `chrono-tz`; "0 3 * * *" means 3 AM in the
  trigger's tz, not the host's.
- Offline machines miss runs (documented); `catch_up: true` fires one
  missed run at boot if `last_run` < previous due time.
- Hot upgrade: trigger state is in `state.json`; the scheduler is a
  thread that dies with exec and re-reads on start (relay-worker
  pattern).

### 4.4 Chaining (the composition payoff)

Event triggers make workflows compose without mule changes:
`workflow A completes → trigger → workflow B` with input templated
from `last_run.output`. Cross-machine chains work the same way (the
trigger targets any of the account's machines; the mirror makes
remote triggers visible everywhere).

## 5. Client surfaces (parity summary)

| surface | workflows | runs | triggers |
|---|---|---|---|
| TUI | `:workflows` CRUD overlay | normal pane (split/scroll/detach) | triggers section in the overlay (cron text field + event picker) |
| Web | Workflows page, full editor | pane in the terminal view | triggers tab with form editor |
| Mobile | list + run + delete | pane + timeline metas | list + enable/disable (edit via web) |

## 6. Acceptance

1. Build a 2-step agent workflow on the web, run it from the phone,
   watch it live on the TUI — same pane, same rows.
2. Cron trigger fires nightly with no client attached; `last_run`
   visible in the dashboard next morning.
3. Chain: workflow A completes → workflow B starts with A's output in
   its input.
4. Webhook (design/webhook-receiver.md) fires a workflow from a
   signed external POST.
