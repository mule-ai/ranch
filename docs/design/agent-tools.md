# Design: The Ranch Agent Tool Surface

*Agents running inside ranch can spawn, steer, observe, and close other
agent panes — orchestration that is visible to the human as ordinary
panes in the layout.*

Status: design (Phase A of PLAN.md).

**Topology note (read first):** the agent runtime is pi *or* forge,
and forge often runs on a **different host** than the ranch daemon
(e.g. forge on the lab box, ranchd on your machine). Every tool call
therefore flows **daemon-initiated** — see §6 for the two transports
(loopback for local-pi agents, forge-hosted bridge for forge agents).

## 1. Why panes are the orchestration unit

Other systems orchestrate sub-agents invisibly: a parent agent spawns a
subprocess, streams flow behind an API, the human sees a log at best.
Ranch already has the right primitive — **a pane is a live, inspectable,
addressable unit of agent work**. When an agent spawns a sub-agent in
ranch:

- the human **sees** the new pane appear (and can watch, read, or type
  into it — steering a sub-agent by hand is one keystroke away),
- the pane is durable (survives detach, Tier-1/Tier-2 restarts),
- closing is explicit and visible,
- and the parent agent gets programmatic access to the same thing the
  human sees — no separate channel of truth.

## 2. Components (two topologies)

The caller agent is either **local pi** (a child of ranchd) or a
**forge agent** (usually on a different host — the lab deployment).
ranchd has no inbound listener and the forge sandbox cannot reach
ranchd's loopback, so remote-agent tool calls flow
**daemon-initiated** through a forge-hosted bridge (§6.2).

```
┌──────────────────────── ranchd (user's machine) ─────────────────┐
│                                                                  │
│  session "work"                                                  │
│  ┌──────────────────┐  ┌─────────────────────────────────────┐  │
│  │ pane A           │  │ pane B (spawned)                    │  │
│  │ agent — LOCAL pi │  │ agent — local pi (or forge)         │  │
│  │ (child of ranchd)│  │                                     │  │
│  │  harness calls   │  │  completion: working→idle           │  │
│  │  ranch_spawn ────┼─▶   turn_ended / agent_end ──────────┐  │  │
│  │  via loopback    │  │                                  │  │  │
│  │  control API     │  │                                  │  │  │
│  └────────▲─────────┘  └──────────────────────────────────┼───┘  │
│           │                                               │      │
│  ┌────────┴───────────────────────────────────────────────▼───┐  │
│  │ agenttools.rs                                               │  │
│  │  • spawn registry (pane → caller, spawn_id, callback state) │  │
│  │  • policy gate (allow / ask / deny)                         │  │
│  │  • loopback control API (local pi agents)                   │  │
│  │  • forge bridge client (remote forge agents, §6.2)          │  │
│  │  • callback delivery: AgentDone frames → caller pane        │  │
│  └────────▲──────────────────────────────────┬─────────────────┘  │
│           │ spawn/approve                     │ AgentDone          │
└───────────┼──────────────────────────────────┼────────────────────┘
            │                          (chat row + harness wake-up)
   ┌────────┴────────┐             forge host (different machine!)
   │ attached clients│                 ┌──────────────────┐
   │ (approve chips) │  SSE ◀──────────┤ forge-api        │
   └─────────────────┘  ranch_tool_    │ queues ranch_*   │
            └─────────────request──────┤ tool requests    │
                 result POST──────────▶└──────────────────┘
```

For a **local-pi caller**, pane A's tools arrive via the loopback
control API (§6.1). For a **forge caller**, the same tools arrive via
the forge bridge (§6.2) — forge queues the agent's tool request,
ranchd sees it on the SSE stream it already consumes, executes it,
and POSTs the result back. Either way the tool executes in
`agenttools.rs`; policy and ownership are enforced in one place.

## 3. The five tools

| tool | args | behavior |
|---|---|---|
| `ranch_spawn` | `kind` ("pi"\|"forge"), `profile_id?`, `name?`, `cwd?`, `prompt`, `mode` ("split"\|"session"), `callback?` | Creates a chat pane running the requested agent with the initial prompt. `split` = new pane in the caller's session (visible next to the caller); `session` = new named session (for bigger fan-outs). Returns `{session, pane, spawn_id}`. |
| `ranch_send` | `pane`, `text`, `delivery` ("steer"\|"queue") | Sends a message to a spawned pane's agent. `steer` → pi RPC `steer` / forge message (interrupts at the next tool boundary); `queue` → pi `follow_up` / forge message queued post-turn. |
| `ranch_status` | `pane` | `{state, busy, model, spawned_at}` — cheap check without reading rows. |
| `ranch_read` | `pane`, `since_seq?`, `limit` | Recent conversation rows (same `ChatMsg` shape clients render). |
| `ranch_close` | `pane` | Closes the pane (kills pi child / unbinds forge watch). Spawn-scoped: only panes in the caller's spawn registry. |

Ownership rule: a tool call from pane A may only `ranch_send`,
`ranch_status`, `ranch_read`, `ranch_close` panes in A's spawn
registry (or A itself). Humans are unrestricted.

## 4. Callbacks (the contract that makes it orchestration)

A spawn without a callback is just remote process management. The
callback contract:

1. `ranch_spawn(callback: true)` registers a waiter keyed by
   `spawn_id`.
2. The daemon watches the spawned pane's agent status. Completion =
   the **working→idle** transition the workers already emit (forge:
   `turn_ended` SSE event → `meta kind="agent" status="idle"`;
   pi-local: `agent_end`/`turn_end` RPC event in `pilocal.rs`).
3. On completion the daemon:
   - broadcasts `AgentDone {spawn_id, pane, session, outcome,
     last_row}` — the caller's chat pane renders a system row
     (`✓ "refactor-agent" finished — last message: …`),
   - wakes the caller's **harness** (path depends on where the caller
     lives — §6):
     - **local-pi caller**: the ranch pi extension long-polls the
       loopback control API (`GET /agent/callbacks?pane=A`); the
       callback returns as the tool result of the long-poll. The
       agent's turn was already "open" from its perspective — no new
       harness machinery needed.
     - **forge caller** (possibly on another host): ranchd POSTs the
       completion to the forge bridge (`POST /ranch-tools/{id}/result`
       with the spawn's request id — the same bridge that delivered
       the tool call, §6.2). Forge resolves the caller's pending
       callback: as a resolved tool result if the harness long-polled
       the spawn, otherwise as a system row via the F3 `notify` hook
       that the harness picks up on its event stream. Fallback if F3
       is late: `ranch_status` polling; the tool contract is
       unchanged.

Timeout policy: `ranch_spawn` takes an optional `timeout` (default:
none — agents can run for hours); on timeout the daemon delivers
`AgentDone {outcome:"timeout"}` and leaves the pane open (the human
decides; a timed-out sub-agent might still be half-right and useful).

## 5. Policy gate

`daemon.toml`:

```toml
[agents]
spawn_policy = "ask"        # allow | ask | deny
max_agent_panels = 8        # concurrent agent-spawned panes per session
```

- `allow` — spawns execute immediately.
- `ask` — daemon broadcasts `AgentSpawnRequest {spawn_id, caller,
  kind, prompt-preview, …}`; every attached client shows an approval
  chip (TUI: status bar `agent requests a pane [y/n]`; web/mobile:
  inline chip with prompt preview). `AgentSpawnApprove {spawn_id,
  allow}` resolves it; unapproved requests expire (5 min) with
  `AgentDone {outcome:"denied"}` to the caller.
- `deny` — spawns fail with a policy error.

## 6. Transport: where the agent lives decides the path

The agent runtime is either **local pi** (a child of `ranchd`, same
host) or **forge** (often a *different host* — e.g. the lab
deployment). So there are two transports, and the nuance matters:
ranchd has no inbound listener, and a forge-sandboxed agent cannot
reach ranchd's loopback. The rule:

> **Every ranch-tool call flows daemon-initiated.** The agent's
> harness never connects to the daemon; connections are always opened
> by the party that already holds the trust relationship.

### 6.1 Local pi agents — loopback control API

`127.0.0.1:<port>`, bearer token minted per daemon, passed to the
child via env `RANCH_CONTROL_TOKEN`:

- pi extensions are JS (no unix sockets without native deps), so HTTP
  is the natural transport; forge-style tool forwarding proved this
  pattern (forge's own `forge-tools` extension is a pi extension using
  `pi.registerToolProvider` and HTTP).
- The API is a thin shim: each route constructs the corresponding
  `Frame` and enters the same `handle_frame` path clients use —
  **one implementation** of spawn/steer/close semantics, two entry
  doors.
- Why not relay frames: agents are local children; loopback keeps the
  blast radius local and avoids spending Realtime quota on tool
  chatter.

### 6.2 Forge agents — the forge bridge (daemon-polled)

A forge agent runs inside forge's sandbox on the forge host. It
registers the same five tools via a pi extension (`ranch-tools`, the
sibling of `forge-tools`), but the tools forward to **forge**, not to
ranchd — and forge does not execute them. Forge is the rendezvous
point both parties already trust (ranchd holds a forge API key + SSE
streams; the agent talks to forge via the tool executor).

```
forge agent (sandbox, forge host)          ranchd (user's machine)
│ pi calls ranch_spawn                           │
│   └─ extension → forge /tools/execute          │
│        forge: record RANCH_TOOL_PENDING        │
│        (per-session queue + SSE event)         │
│        handler LONG-POLLS the queue row        │
│                                                │
│                     ┌── SSE: ranch_tool_request ◀┤ (the existing
│                     │                            │  forge worker
│                     ▼                            │  stream)
│              ranchd executes the tool            │
│              (spawn pane, etc.)                  │
│                     │                            │
│   POST result ◀── POST /ranch-tools/{id}/result ─┘
│        │          (machine JWT or forge key +
│        │           session ownership check)
│        ▼
│   forge returns the result to the long-polled
│   /tools/execute call → agent's turn continues
```

Why this shape:

- **No inbound path to ranchd.** The forge worker already consumes
  `GET /sessions/{id}/events` (SSE) for every watched pane; forge
  publishes a `ranch_tool_request` event on that same stream and
  queues the request. ranchd executes and POSTs the result back to
  `POST /ranch-tools/{id}/result` — authenticated with the forge
  credential it already holds, authorized by checking the session is
  watched/bound to this machine.
- **The agent's turn stays synchronous.** forge's `/tools/execute`
  handler for `ranch_*` tools long-polls the pending-request row
  (bounded, e.g. 60 s — spawn/steer are fast local ops; a spawn under
  `ask` policy that isn't approved in time returns "denied/timeout",
  which the agent handles like any tool error). No fire-and-forget
  ambiguity.
- **Callbacks flow the same bridge.** `AgentDone` (spawned pane's
  working→idle transition) is delivered by ranchd the same way: it
  POSTs a result/notification to forge, which (F3) resolves the
  pending callback into the caller's session — as a system row the
  harness hands to pi, or as the resolved tool result when the caller
  used the long-poll `ranch_wait` variant of spawn.
- **Cross-host by design.** A forge-hosted agent can spawn *local pi*
  panes (they run on ranchd's host, next to the human's shells) or
  *other forge* panes (agents on the forge host). Both are ordinary
  panes on the ranchd machine; only the agent harness's location
  differs.

Forge changes required (Phase B forge items, additive):
- **F3a**: pending-request queue + SSE event + result endpoint +
  long-poll on `/tools/execute` for the `ranch_*` tool names (or a
  dedicated `/ranch-tools/*` surface — same shape).
- **F3b**: callback delivery into a session (the `notify` hook —
  `POST /sessions/{id}/notify` persists a system row + bus publish).

### 6.3 Mule-hosted agents

Mule runs pi-in-RPC-mode for workflow agents on mule's host — also
remote. v1 gives mule agents **no ranch tools** (mule has its own
orchestration model; workflows are how mule agents compose). If we
later want mule agents in on it, the forge bridge pattern applies
unchanged (mule queues, ranchd polls via the mule worker).

### 6.4 Policy is daemon-side, always

Whichever transport, the tool executes **in ranchd** — policy gate,
spawn ownership, and `AgentDone` emission all live in one place
(`agenttools.rs`). The transports differ only in how the request
arrives and the result returns. AuthZ on the result path: ranchd's
POSTs to forge carry the forge API key + target session id; forge
verifies the requesting session is owned by the same user/tenant —
so a forge agent cannot steer panes on a machine that isn't watching
its session.

## 7. Rendering (all clients)

- Spawned panes are ordinary panes: TUI shows them in the sidebar/tree
  with a ⚡ badge; web/mobile show the badge on the pane title.
- `AgentDone` renders as a system chat row in the caller pane (dim,
  prefixed ✓/✗).
- Approval chips: TUI status-bar prompt (y/n), web + mobile inline
  component with prompt preview and Approve/Deny.
