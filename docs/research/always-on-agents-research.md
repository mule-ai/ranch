# Always-On, Always-Learning Agents — Research & Concept

> Implementation plan: `docs/PLAN-HERD.md` (this doc stays untouched —
> it's the concept record).

*Working title for the effort: **"Herd"** — many agents, always on, always
learning, in your corral (ranch).*

Status: research synthesis. Grounded in (a) the OpenAI **Dots** announcement
and its companion safety post, (b) the **Pi Durable** RFC from Earendil, and
(c) a first-principles audit of our own tooling in `~/src/`: **forge**,
**mule**, **ranch** (+ `pi-acp`, forge's voice/embedding/router surfaces,
the matrix appservice integration, and the `lab` host infra).

Companion reading in this repo:

- **`docs/PLAN-HERD.md`** — the implementation plan this research
  became (phases H0–H6, task-level detail, status ledger).
- `docs/SPEC.md` — ranch target system (four-tool model)
- `docs/design/agent-tools.md`, `docs/design/workflows-and-triggers.md`,
  `docs/design/webhook-receiver.md`, `docs/design/agent-builder.md`

---

## 1. Executive summary

OpenAI's Dots [1] define the product category we are building toward:
**remarkably capable, always-on agents that learn from feedback over time,
work toward goals 24/7, live on their own computer, and reach the user on
every surface they use** — chat, voice calls, Slack/Teams — while a
safety layer (auto-review, custom rules, read-only proactive research)
keeps autonomous action trustworthy.

We are unusually well positioned to build an open version of this. We
already own every layer:

| Dots capability | Our asset | Gap |
|---|---|---|
| Durable agent that survives crashes | **forge** (Postgres source-of-truth, pi subprocess per session, durable resume) | Forge's durability is *turn-level and filesystem-replay based*; pi-durable shows a *task/checkpoint* model is simpler and stronger. Forge also lacks a scheduler, wake-on-event, and inter-agent messaging inside forge itself. |
| Orchestration, background work, async | **mule** (workflows, jobs, WS hub, WASM steps) | Mule has no long-lived conversational state and no notion of an agent that persists between jobs; today it *drives* agents but doesn't *host* them. |
| Reach the user everywhere; inspect the agent's computer | **ranch** (TUI/web/Android over one daemon; chat panes, workflow panes, agent-spawning tools) | No agent notifications on mobile yet (native app has the foreground-service plumbing; needs the product surface), no voice *input* on mobile, no proactive/"intrusive" presentation model. |
| Learns your preferences over time | forge session embeddings + semantic router (Qwen3 embed/rerank); pi skills; heartbeat.md pattern | There is **no memory system**. Learning is the single biggest greenfield build: memory stores, reflection loops, preference extraction, distillation across agents. |
| Voice calls | forge `/v1/audio/*` proxies (Parakeet STT, Kokoro TTS) | Voice exists as *proxy endpoints*, not as a full-duplex agent loop with barge-in and telephony surfaces. |
| Own cloud computer + connect to your devices | forge nspawn sandbox + per-session rootfs; ranch daemon as the *user's device* bridge | The sandbox is operator-gated and host-bound; dots-style "connect your laptop" is exactly ranchd's daemon model, but it isn't framed or permissioned as an agent-accessible device yet. |

**The core architectural thesis:** pi-durable [2] has already solved —
elegantly, at ~15k LOC — the *harness* problem we would otherwise rebuild:
conversations as durable tasks, ownership trees, hooks, documents,
compaction, multiplayer views, malleable extensions. Our forge replicates
perhaps 40% of it with more code and more complexity (filesystem replay,
dual-writer audit log, idle-timeout process babysitting). The right move
is a **forge refactor that adopts pi-durable's semantic model but keeps
Postgres** (see §8 — and note pi-durable's storage interface was
explicitly designed to be reimplemented on "a key-value store or
Postgres" [2], so this is a port, not a fight).

On top of that refactored harness, the always-on/always-learning layer is
three new subsystems plus orchestration glue:

1. **Memory & learning** (greenfield, the differentiator): episodic +
   semantic + preference memory in Postgres/pgvector, written by
   **reflection workflows that are themselves mule workflows** — the
   system eats its own dog food.
2. **Proactivity engine**: wake conditions (cron, event, webhook, agent
   signal, memory trigger) compiled to pi-durable-style background
   tasks; a "proactive research" mode restricted to read-only tools
   exactly as Dots does [1][3].
3. **Presence & voice**: ranch as the universal presence layer — every
   agent reachable from TUI/web/Android; mobile gains agent event
   notifications and voice input; forge's STT/TTS proxies become a
   proper voice loop.

Mule becomes the **behavioral constitution and learning-loop engine**:
it declares *when* agents wake, *which* workflows run on their behavior
(especially reflection/learning), and enforces async boundaries (voice,
webhooks, long jobs) that a conversational harness should not block on.

The rest of this document is the evidence and the design.

---

## 2. The target concept: OpenAI Dots (annotated)

Source: "Introducing dots," OpenAI, 2026 [1]; safety companion [3].

### 2.1 What a dot is

> "Dots are remarkably capable, always-on agents built to handle
> everything… Powered by GPT-6 Astra, they have their own cloud computer,
> learn from feedback over time, and can work towards your goals 24/7."
> [1]

Key product properties, decomposed:

1. **Always-on** — "work towards your goals 24/7"; "When you aren't
   actively working with it, your dot looks for ways to help in the
   background. We call this 'proactive research'." [1]
2. **Own computer** — "Each dot works on its own cloud computer, while
   your computer and its contents stay separate unless you choose to
   connect it." [1] Plus "its own browser, and the apps you've
   connected" (4,000+ via plugins). [1]
3. **Learns from feedback over time** — "The more you work together, the
   more your dot learns your preferences, how you think, and what good
   looks like to you." [1] Also "Dots build context over time… Each dot
   has its own context that you can reset at any time." [3]
4. **Omnipresent** — "Your dot is always within reach, whether it is
   through ChatGPT, Slack, or Teams… or you can simply hop on a voice
   call… Dots carry context across every channel." [1]
5. **Inspectable** — "You can open your dot's computer at any time to
   inspect its work." [1] Activity View "lets you follow and guide
   ongoing work… add context, correct a misunderstanding, change
   direction, or ask a dot to stop." [3]
6. **Multi-agent** — "Over time, we envision teams of dots working
   together on your behalf" [1]; today the primary dot delegates to
   sub-agents ("When they delegate work, the other agents are instructed
   to keep the information needed for the task" [3]).
7. **Specialist dots** — org-scoped agents with "their own identity,
   credentials, and access to the systems it needs… take on
   well-defined responsibilities" (procurement, invoicing, support,
   contracting pilots). [1]

### 2.2 The safety/behavior model (this is half the product)

From the safety post [3]:

- **Auto-review** — a *separate* safety system checks consequential
  planned actions (sending email, changing files) against the user's
  instructions and custom rules *before* they run; blocks come back to
  the dot with a reason. "We keep the controls that enforce Auto-review
  outside the environments dots can change."
- **Custom Rules** — user-authored action policy ("never send emails",
  "require approval for purchases"); dots can help write them but need
  approval to change them.
- **Tiered sensitivity** — health data needs a *named* recipient;
  low-sensitivity data a *class* of recipient; purchases always need
  approval; password changes and money transfers are handed back to the
  human entirely.
- **Proactive research is read-only by construction** — "We enforce
  these limits in code: the research tasks cannot directly send messages
  to other people, change content in connected apps, or control a
  browser or desktop."
- **Prompt-injection defense** — model training + tool restrictions +
  pre-action checks + behavioral monitoring that "can pause that work
  and show you a warning."
- **Credential isolation** — "Secure sign-in… pauses the model while
  you complete a secure login form… passwords stay outside the model's
  context"; saved passwords via "a dedicated encrypted credential
  service."

**Design lesson for us:** the always-on behaviors and the enforcement of
behavior must live *outside* the agent that is being constrained — a
separate control plane. This maps directly onto **mule as the policy/
orchestration plane** and ranch's daemon-authoritative tool surface
(agents can only act through tools the daemon mediates; ownership and
approval chips are daemon-side — ranch SPEC §5.3).

---

## 3. Pi Durable: the harness reference model (annotated)

Source: "Pi Durable," Earendil, 2026-10-01 [2]. Full RFC read; this
section captures what matters for our refactor decisions.

### 3.1 Core vocabulary

- **Harness** = storage + machinery to run many conversations in
  parallel, with tools and execution environments. ~15,000 lines total.
- **Conversation** = durable transcript + agent config (model, thinking
  level, extensions/tools, instructions, cwd).
- **Task** = every unit of execution (model request, tool call,
  compaction, or app-defined) with a checkpoint per step.
- **Extension** = named bundle of prompt sections, tools, hooks, tasks;
  conversations store *names*, never code → malleable hot-swap.

### 3.2 The properties we want (and their mechanisms)

| Property | pi-durable mechanism [2] |
|---|---|
| Survive crashes | Every step checkpoints; `harness.resume()` finds unfinished tasks; model requests re-sent; interrupted tool calls rerun only if `replay: "safe"`, else the model is *told* it was interrupted; `requestId` gives exactly-once submissions. |
| Many conversations, one process | One harness, N concurrent conversations; a conversation **forks** another at any transcript point (sees parent history, no copy). |
| Ownership tree | Tasks and conversations form one ownership tree; aborting a task aborts what it owns bottom-up; **background tasks** outlive the turn (reminders, detached subagents) — `ownership: conversation, background: true`. |
| Subagents | A few lines: a tool creates a conversation owned by the calling task, configures a smaller model/no tools, waits; UI shows it under the call via `api.details`. |
| Long conversations | Compaction is a background *task*; summary lands at the next turn boundary; `reset()` from a handoff note; `search_history` over everything ever stored (nothing deleted). |
| App state as durable as the agent | **Documents**: typed JSON in the same atomic commits as the transcript; fork semantics per doc (`asOf`/current/fresh); prompt sections can render docs; UIs subscribe. |
| Behavior injection | Extensions define prompt sections rebuilt every request (changed sections cache-invalidate only the delta); **hooks** chain over built-in tasks (`beforeTool` can block; decisions memoized so retries don't re-ask approvals). |
| Multiplayer | All UI state is committed state; clients get a current view + delta subscription; anyone can steer or queue (`whenBusy: "steer"`). |
| Malleability | Registry installs replace by name while running; running calls finish on old code; next call uses new code. |

### 3.3 What pi-durable does *not* give us (the gaps we fill)

- **No scheduler / wake-on-event / integrations.** Nothing in the RFC
  cron-ticks, listens for webhooks, or watches files. That is the
  always-on layer — ours to build (mule + ranch triggers).
- **No memory/learning.** Documents and compaction summaries are the
  only long-term state, and they are per-conversation. Cross-conversation,
  cross-agent, cross-time memory is greenfield (§6).
- **No multi-surface presence, no mobile, no voice.** It's a library.
  Ranch + forge's voice proxies are our surface layer.
- **No multi-tenancy, auth, or quotas.** Forge already has users, API
  keys, argon2 auth, row-level tenancy [4]. Keep it.
- **TypeScript/Node-only, SQLite/JSONL/memory storage.** Single-process
  ownership of storage ("One process owns a storage at a time, other
  clients attach to that process") — fine for one agent, thin for a
  fleet. Postgres gives us multi-writer safety, pgvector, LISTEN/NOTIFY,
  and operational maturity we already run for forge and mule.

**On the sqlite-vs-postgres question you raised:** pi-durable's own
storage abstraction is deliberately small ("easy to implement on top of
whatever you have, like a key-value store or Postgres" [2]) and ships a
conformance suite + benchmarks for third-party backends [2]. A Postgres
backend is an intended configuration, not a workaround. We keep Postgres
because (a) forge's entire durability claim already rests on it, (b)
fleet-scale agents need concurrent access + operational tooling, and (c)
pgvector is the natural home for the memory system (forge currently
stores embeddings as `REAL[]` with in-app cosine only because pgvector
isn't installed on one host [5] — fix the host, not the design).

---

## 4. Audit of our existing tooling

### 4.1 Forge — the durable agent platform (`~/src/forge/`)

**What it is.** Rust (axum + sqlx + tokio) API server; one long-lived
`pi --mode rpc` Node subprocess per session; every user/assistant/tool
row persisted to Postgres with a monotonic per-session sequence; SSE
event streaming; multi-user tenancy; nspawn sandbox; OpenAI-compatible
surface; STT/TTS proxies; embedding-based semantic message router [4][6].

**Strengths we build on:**

- The `messages` table as source of truth; durable resume by replaying
  tool calls (`resume.rs`) and rebuilding context via generated session
  jsonl (`session_replay.rs`) [4].
- The harness/executor **ToolRecorder split** — harness writes call
  rows, executor writes result rows, linked by `tool_call_id` [6]. This
  is proto-durability thinking already.
- Real ops maturity: self-update endpoint, metrics, per-call timing,
  integration + e2e suites, nspawn per-session rootfs with pinned Rust
  toolchain [4].
- Voice backends exist: `POST /v1/audio/transcriptions` (Parakeet STT)
  and `/v1/audio/speech` (Kokoro TTS), OpenAI-compatible [7].
- The semantic router (`POST /router/message`): embed → cosine top-K →
  Qwen3 rerank → route to best session or start new [5][8]. This is a
  *memory-adjacent* primitive we can generalize.
- Proven scheduled-agent pattern: systemd timers + per-agent profile +
  `heartbeat.md` + Matrix room [9] — a working, if clunky, always-on
  precursor.

**Shortcomings vs the dots concept:**

1. **Turn-centric, not life-centric.** A forge session exists between
   `POST /messages` calls. There is no way for a session to *wake
   itself*, subscribe to events, or hold background work. The scheduled-
   agents doc works around this with systemd timers outside forge [9] —
   the tell that the scheduler belongs inside.
2. **Durability is heavier than it needs to be.** Resume requires
   *re-executing prior tool calls* to rebuild the working tree [4] —
   side-effecting replays with all the idempotency hazards pi-durable
   solves with explicit `replay: safe` declarations and task
   checkpoints [2]. The harness/executor dual-writer split, while
   elegant, exists because tool execution is out-of-band over HTTP from
   a TypeScript extension [4][6]; pi-durable runs tool calls as
   in-harness durable tasks with intent recorded before execution [2].
3. **No ownership trees / subagent semantics.** Subagent coordination
   currently lives in *ranch* (agent tools) [10]; forge can't natively
   express "abort this turn and everything it owns."
4. **No compaction story.** Long sessions depend on pi's own context
   handling; no background summarization task, no handoff/reset
   machinery [2 §Compaction is the target].
5. **No app-state documents.** Structured durable state (todos, plans,
   memory notes) would today be crammed into message rows.
6. **Extensions are one bash-built TS file** (`extensions/forge-tools`)
   that forwards tools back over HTTP [4] — no registry, no hooks, no
   per-conversation tool selection.
7. Complexity concentration: `api/mod.rs` is "the harness event loop"
   *and* the HTTP surface *and* turn plumbing [6] — the refactor in §8
   separates these.

### 4.2 Mule — the workflow platform (`~/src/mule/`)

**What it is.** Go; PostgreSQL-backed; primitives are providers, skills,
WASM modules, agents (pi RPC runtime), workflow steps (agent | wasm),
workflows; job queue with worker pools; WS hub for streaming; drag-and-
drop workflow UI; OpenAI-compatible API as the main interface
(`model: "agent/x" | "workflow/x" | "async/workflow/x"`) [11][12].

**Strengths:**

- Durable job queue with `QUEUED→RUNNING→COMPLETED|FAILED` lifecycle,
  cancellation, worker pools, per-step `JobStep` input/output records
  [10][12].
- WASM (wazero) for deterministic non-LLM steps; agents for LLM steps —
  the right split for **learning loops** (deterministic extraction,
  embedding, storage steps + LLM judgment steps).
- Already integrated with ranch as first-class panes + triggers
  (cron/event/webhook fire mule jobs) [10].
- Owns the async surfaces a conversation loop shouldn't block on.

**Shortcomings:**

1. **Stateless about agents.** An agent is config (provider, model,
   system prompt, skills) consumed per step [12]; there's no durable
   conversational identity a workflow can *continue* — every run is a
   fresh context. Always-on agents need workflows that operate *on*
   long-lived conversations (feedback them, compact them, reflect on
   them).
2. **No event subscription model for agents.** The WS hub is a global
   firehose with no per-job subscription (ranch filters server-side)
   [10]; fine for panes, insufficient as an agent wake bus.
3. **No durable timers beyond what ranch's trigger scheduler provides**
   externally [10] — reminders that fire tomorrow, retry policies,
   "check back in 3 days" need harness-level durable timers
   (pi-durable's task timers [2]).
4. **No memory primitives** — nowhere to put learned state.
5. **Auth/multi-tenancy thinner than forge's** (no per-user API keys at
   forge's level) — matters when agents from many users share one
   platform.

### 4.3 Ranch — the surface (`~/src/ranch-2/`)

**What it is.** Terminal multiplexer + daemon (`ranchd`) + three clients
(TUI ratatui, web React, and a **native
Kotlin** Android client); server-side ghostty-vt terminal emulation; chat panes
over forge or local pi; workflow panes; trigger scheduler (cron/event/
webhook); webhook relay via Supabase edge function; **agent tools**
(`ranch_spawn/send/status/read/close/ask`) with daemon-side ownership,
completion callbacks, and an approval-capable `ranch_ask` [10][13].

**Strengths:**

- The agent-tools surface is genuinely ahead of the dots concept in one
  way: *agents orchestrating agents visibly* — every subagent is a real
  pane a human can watch, and callbacks mean "the caller never polls"
  [10]. This is our multi-agent story's foundation.
- Trigger scheduler already does cron + event + webhook with
  `state.json` persistence and hot-upgrade safety [10].
- Native Android client already solved the hard notification problem:
  "A foreground service holds the Supabase Realtime socket so agent-event
  notifications fire in the background" [14]. The plumbing for dots-style
  mobile presence exists; it needs product surface (agent identity,
  notification actions, voice).
- `ranch_ask` is a human-in-the-loop primitive with TTL and
  first-answer-wins semantics across all clients [10] — the approval UI
  for auto-review-style gates is 80% there.

**Shortcomings:**

1. **No agent identity/notifications product surface.** Notifications
   are pane turn-end events [14]; there's no "your agent finished the
   invoicing task" notification with actions (approve/redirect/stop),
   no per-agent mute/priority, no notification *originated by an agent's
   decision to involve you* (dots: "They can also message you with
   progress, questions, or decisions that need you" [1]).
2. **No voice input on mobile**; voice exists only as forge proxy
   endpoints [7].
3. **No Activity View.** Background/proactive work needs a cross-agent
   activity timeline (dots' Activity View [3]); ranch's dashboard is
   session/pane-oriented, not work-oriented.
4. **Chat pane ≠ agent.** Ranch conflates *pane* (surface) with *agent*
   (identity). A dot is an entity you can reach from anywhere; a ranch
   chat pane is one window onto one session. Needs an agent registry
   layer (§7.1).
5. Local-pi panes and forge panes have different durability properties;
   the always-on concept should standardize on the durable one.

### 4.4 Supporting cast

- **pi-acp** — ACP adapter for pi (Zed integration); proves pi's RPC
  protocol is bridgable to other protocols cheaply; relevant if we ever
  expose agents to editor clients [15]. Not on the critical path.
- **forge semantic router + embeddings** (Qwen3-Embedding-4B /
  Qwen3-Reranker-4B via Bifrost) [5][8] — the retrieval engine for
  memory routing; currently routes *messages to sessions*; will route
  *memories to context*.
- **search tool** (SearXNG metasearch CLI in the sandbox) [16] — the
  read-only web access for proactive research.
- **matrix_appservice** + scheduled agents [9] — existence proof of
  agent-as-entity-with-a-room; the "reach the agent on any channel"
  pattern, today via systemd timers and per-agent setup scripts.
- **lab** — host infra (Nix, services, embeddings stack under
  `lab/ct/embeddings` [8]).

---

## 5. Gap analysis: dots capability → our stack

| # | Dots capability [1][3] | Today | Gap severity | Where it should live |
|---|---|---|---|---|
| 1 | 24/7 goal pursuit | systemd-timer heartbeats [9] | ★★★ | forge (durable tasks, timers) |
| 2 | Own computer, inspectable | nspawn session rootfs [4]; ranch panes | ★ | forge sandbox + ranch (already close) |
| 3 | Learn preferences/feedback | nothing persistent | ★★★ | new memory subsystem (forge + mule loops) |
| 4 | Proactive research (read-only background) | nothing | ★★★ | forge background tasks + mule policy |
| 5 | Reach on any surface, context carried | ranch 3 surfaces, but pane-scoped | ★★ | ranch agent registry |
| 6 | Voice calls | STT/TTS proxies [7] | ★★ | ranch voice loop over forge audio |
| 7 | Auto-review / custom rules | `ranch_ask` approval chip [10] | ★★★ | mule policy engine + ranch approval UI |
| 8 | Activity View | dashboard per pane | ★★ | ranch activity timeline |
| 9 | Teams of dots / delegation | `ranch_spawn` tools [10] | ★★ | forge ownership trees + ranch visibility |
| 10 | Specialist dots (identity, credentials, systems of record) | forge profiles + per-user tenancy [4] | ★★ | forge agent entities + credential service |
| 11 | Crash-proof mid-action | fs replay [4] | ★ | forge refactor (§8) |

---

## 6. The learning problem (the actual differentiator)

Nobody's harness does this; OpenAI's marketing claims it ("learns from
feedback over time" [1]) without disclosing mechanism. We get to design
it. Proposed memory architecture, all in Postgres:

### 6.1 Memory tiers

1. **Working context** — the live transcript + compaction summaries
   (pi-durable-style [2]).
2. **Episodic memory** — per-agent, append-only record of *completed
   work*: tasks, outcomes, user feedback (explicit 👍/correction and
   implicit edits/rejects), with embeddings. Rust table sketch:
   `episodes(agent_id, conversation_id, task_ref, summary, embedding,
   feedback, ts)`.
3. **Semantic memory** — distilled facts/preferences: "user prefers
   Conventional Commits", "invoices go out on the 1st", "never deploy
   Friday". Versioned, sourced (which episodes produced it), with
   confidence and *decay*. `beliefs(agent_id, kind, content, embedding,
   confidence, source_episodes[], updated_at)`.
4. **Shared/team memory** — dots' specialist-dots "systems of record"
   [1]: memory scoped to a household/org of agents, with per-agent
   write policy. `org_beliefs` + ACLs (forge tenancy model [4] extends
   naturally).

pgvector (HNSW) for retrieval; reuse the forge embed/rerank stack [5][8];
the semantic router generalizes into `route(memory_query) → top-K`.

### 6.2 Learning loops = mule workflows over the agent

The key move that makes this *our* architecture rather than a
research project: **reflection runs as mule workflows, so learning is
user-inspectable, editable, versionable, and triggerable** — the same
way every other behavior is.

- **Trigger** (ranch trigger or mule cron): "after N feedback events"
  / "nightly" / "on explicit 'remember this'".
- **Steps**: deterministic WASM steps pull recent episodes + feedback;
  an agent step (cheap model) proposes belief updates as structured
  diffs; a validation step (WASM or second agent) checks contradictions
  and confidence; a write step commits to `beliefs` with provenance.
- **Governance**: belief changes surface as ranch notification cards
  ("I've learned: you prefer brief replies. Keep / Forget") — mirroring
  dots' "dots can help write Custom Rules but need approval to change
  them" [3], and reusing `ranch_ask`'s approval chip [10].
- **Prompt injection**: agent prompt sections render the agent's own
  beliefs (pi-durable sections read docs each request [2] — the same
  pattern, backed by memory tables).

### 6.3 Cross-agent learning

- Agents share a message bus (forge `MessageBus` is in-process [6];
  needs Postgres LISTEN/NOTIFY or the Supabase relay for cross-host).
- Primitive: `agent_signal(from, to, kind: handoff|insight|request)`.
  A specialist invoicing dot can publish "this user's PO template
  changed" to the org memory; other agents retrieve it at need.
- Delegation already exists (`ranch_spawn` + callbacks [10]); add
  shared-memory read scopes so a delegate learns *from* the delegator's
  episode without seeing its transcript (dots' "keep the information
  needed for the task and avoid retaining unnecessary sensitive details"
  [3]).

---

## 7. Proposed concept architecture ("Herd")

```
                       ┌──────────────────────────────┐
                       │  Supabase (cloud)             │
                       │  auth · relay · webhooks ·    │
                       │  push notifications           │
                       └──────▲───────────────┬───────┘
      TUI ──────┐             │ frames        │ webhook → relay
      Web ──────┼── clients ──┘               ▼
      Android ──┘        ┌──────────────────────────────┐
                         │ ranchd (user's machine)      │
                         │  panes · activity view ·     │
                         │  approval UI · voice I/O ·   │
                         │  triggers (cron/event/hook)  │
                         └───┬──────────────────────┬───┘
                             │ REST/SSE             │ REST/WS
                  ┌──────────▼─────────┐   ┌────────▼─────────┐
                  │ forge (refactored) │   │ mule             │
                  │  agent registry    │   │  wake policies   │
                  │  durable conversa- │   │  workflows:      │
                  │  tions + tasks     │◄──┤   reflection     │
                  │  tools/hooks/docs  │   │   memory update  │
                  │  memory tables     │   │   reporting      │
                  │  (pgvector)        │   │  policy engine   │
                  └────────────────────┘   └──────────────────┘
                         Postgres (forge DB)  +  Postgres (mule DB)
```

**Division of labor (the one-sentence version):** *forge hosts the
agents' lives, mule scripts their behavior, ranch is everywhere they
can be seen and heard.*

### 7.1 New concept: the Agent entity (distinct from session and pane)

- `agents(id, name, avatar, owner, home_machine, primary_profile,
  visibility, memory_scope)` in forge. A dot. Sessions are
  *conversations the agent has*; panes are *where humans watch*. One
  agent, many conversations, many surfaces — fixing ranch's pane≡agent
  conflation (§4.3.4) and forge's session≡agent conflation.
- Specialist dots = agent + credentials scope + tools allowlist + org
  memory read/write policy [1].

### 7.2 Always-on behaviors (forge's new background-task model)

Port pi-durable's task machinery [2] into forge (§8), then:

- **Durable timers** — agent (or workflow) schedules "wake me when…";
  forge task timer fires a prompt into the conversation.
- **Wake conditions** — ranch's trigger scheduler [10] becomes forge's
  event source: cron, webhook (already relayed [13]), file change,
  agent turn ended, **memory trigger** (new episode matches a belief's
  "watch" predicate).
- **Proactive research** — a background task kind whose *tool registry
  is read-only by construction* (search CLI [16], read, file read;
  writes to private notes only). Enforcement is structural (the
  registry handed to the task literally has no write tools), matching
  dots' "we enforce these limits in code" [3]. Output surfaces as a
  suggestion card, never an unsupervised action.

### 7.3 The approval / policy layer (auto-review, ours)

- **Policy documents** in mule (custom rules): typed JSON, agent-editable
  only via a workflow that requires human approval [3].
- **beforeTool hook chain** in forge (pi-durable hooks [2]) evaluates
  policy: cheap rules in WASM/hook code; ambiguous ones call a reviewer
  agent; blocking ones create a `ranch_ask` approval chip with
  context; decisions memoized (first write wins [2]) so retries don't
  re-ask.
- **Sensitivity tiers** ported from dots [3]: named-recipient for
  sensitive data, class-recipient for mundane, always-ask for
  destructive/irreversible, hand-back-to-human for credentials and
  money movement.
- **Monitor agent** (optional): consumes the audit log [6] and can
  pause a task (dots' monitoring "can pause that work and show you a
  warning" [3]).

### 7.4 Presence & voice

- **Agent registry in ranch**: pick-any-surface "talk to Iris" —
  routes to the agent's most recent conversation or opens a forked one
  (pi-durable fork semantics [2]: fork at any point, see parent
  history).
- **Mobile**: agent-scoped notifications with actions (approve /
  redirect / stop) over the existing foreground-service Realtime
  socket [14]; voice input via the Android speech recognizer → forge
  STT proxy [7] → send as message; optional TTS playback of responses
  (Kokoro [7]).
- **Voice calls**: a mule-orchestrated voice loop — streaming STT
  (Parakeet), agent turn, streaming TTS (Kokoro), with the harness
  treating each utterance as a steer message so the agent can be
  mid-task. Full duplex/barge-in is a v2; push-to-talk phone call is
  v1.
- **Activity View**: ranch dashboard tab listing all background tasks,
  proactive research, pending approvals, recent memory changes —
  across agents (dots' Activity View [3]).

---

## 8. The forge refactor: adopting pi-durable semantics on Postgres

Scope: reorganize forge's core around conversation/task durability
instead of process babysitting + filesystem replay. This is the
"lower complexity" refactor you asked for.

### 8.1 What gets *deleted*

- `resume.rs` (re-execute prior tool calls to rebuild working tree [4])
  and `session_replay.rs` (rebuild pi jsonl [4]) — replaced by task
  checkpoints and transcript-as-truth; tool side effects are *declared
  safe/unsafe* rather than replayed blindly [2].
- The dual-writer harness/executor split [6] — tool calls become
  in-harness durable tasks (intent stored before execution [2]); the
  forge-tools HTTP callback loop [4] disappears for in-process tools.
- 30-min idle cleanup / process-keepalive logic in `session_manager.rs`
  [4][6] — conversations are rows, not processes; a cheap worker
  materializes an active pi subprocess only when a turn runs (or we go
  further: drive the LLM loop in Rust via pi-ai concepts, dropping the
  Node subprocess for headless agents — see open questions §10).

### 8.2 What gets *added*

- **Tables**: `conversations` (agent config as row: model, thinking,
  extension/tool names, cwd — names not code [2]); `tasks` (type,
  state, checkpoint JSONB, owner (conversation/task tree), timers);
  `documents` (typed JSONB, fork semantics); `tool_intents` (pre-exec
  record + replay safety flag + memo).
- **Ownership tree** with bottom-up abort [2]; **background tasks**
  with conversation-scoped ownership.
- **Hooks** (`before_tool`/`after_tool`/`on_yield` chains, memoized
  decisions) and **prompt sections** rebuilt per request from
  extensions + documents + memory (§6).
- **Compaction** as background task; `reset(note)`/handoff; history
  search [2].
- **Postgres storage backend** implementing pi-durable's small storage
  interface + conformance suite [2] — this doubles as our porting
  spec. Consider *actually depending on pi-durable* (npm) for the
  conversation engine in a Node sidecar while Rust keeps the API/
  tenancy/memory layers — vs. reimplementing in Rust. Trade-off in
  §10.
- **Wake/timer service** (§7.2).
- Keep: tenancy/auth [4], SSE events [4], OpenAI surface, voice
  proxies [7], sandbox [4], metrics.

### 8.3 Why this lowers complexity

Today one agent turn touches: HTTP handler → harness loop in
`api/mod.rs` → pi subprocess RPC → forge-tools extension HTTP callback
→ executor → recorder → DB, with resume requiring *re-execution of
tools* [4][6]. After: HTTP handler → task enqueue → durable task
executor (checkpoint/commit) → DB. One writer, one truth, one
abstraction for subagents/timers/compaction/approvals. Forge's own
CHANGELOG trajectory (harness-vs-executor races, dropped-row bugs [4])
is evidence the split was compensating for the wrong process boundary.

---

## 9. What mule becomes

1. **Wake-policy engine**: a `wakes` primitive — declarative conditions
   (cron, webhook, event, memory-trigger, agent-signal) that submit
   prompts/tasks to forge conversations. Ranch triggers remain the
   machine-local edge; mule wakes are the fleet-level superset.
2. **Behavior workflows**: reflection (§6.2), reporting ("prepare the
   invoice, then ask for approval" — dots' tester story [1]), triage
   (Slack bug → investigate → open PR, dots' internal examples [1]).
3. **Policy/custom-rules store + evaluator** feeding forge hooks (§7.3).
4. **Async glue**: voice sessions, webhooks, long WASM jobs — the
   things a conversation must not block on (your framing, confirmed by
   mule's job/worker/WS architecture [11][12]).
5. **Gaps to close**: per-conversation addressing from workflows (new
   `forge_conversation` step type + durable conversation ids),
   durable timers inside mule or delegated to forge timers (§4.2.3),
   per-job WS subscriptions, tenancy parity with forge.

---

## 10. Risks & open questions

1. **Rust reimplementation vs Node sidecar for the durable harness.**
   Reimplementing pi-durable semantics in Rust (forge-native) is more
   code but one runtime, one tenancy model, direct pgvector/memory
   access. A Node sidecar running pi-durable directly gets the
   semantics *for free* + upstream maintenance, at the cost of a second
   runtime, IPC, and a TS/Rust schema boundary. pi-durable ships ~15k
   LOC designed for agent comprehension [2] — a Rust port is feasible
   but is a real project. **Recommendation:** prototype the Postgres
   storage backend against pi-durable's conformance suite first (days,
   not weeks) and let that experience decide.
2. **pi coupling.** forge pins exact pi versions [4]; pi-durable is
   experimental with API instability [2]. Pin aggressively; wrap the
   surface we use.
3. **Runaway proactivity.** Always-on agents that wake themselves can
   burn resources or annoy. Notification digests + a global "activity
   kill switch" in Activity View.
4. **Prompt injection at fleet scale.** One compromised specialist dot
   with org-memory write access is a lattice to pivot through. Mitigate:
   memory writes go through reflection workflows with provenance,
   never direct from tool results [3's "information brought into an
   eligible conversation" containment logic]; belief diffs are
   reviewable.
5. **Supabase dependency in ranch's relay** [10][13] — fine for now
   (it's our product's cloud layer), but the wake-path latency and
   reliability of agent notifications ride on it; self-host option
   exists.
6. **Voice quality/latency.** Parakeet/Kokoro are self-host and good,
   but full-duplex conversation is a different bar than proxies [7];
   keep v1 push-to-talk.
7. **Scope discipline.** Ranch SPEC tenets [10] say shell/multiplexer
   first. All of this is additive pane types + workers; the multiplexer
   core must not regress (the hot-upgrade constraint in PLAN.md
   conventions [13] applies to every new worker).

---

## 11. Suggested build order

1. **P0 — pi-durable spike**: Postgres storage backend + conformance
   suite; run one always-on demo conversation surviving kill -9.
2. **P0 — Agent entity + registry** (forge) + ranch "talk to agent
   anywhere" (fork/continue semantics).
3. **P1 — Task/timer/ownership machinery** in forge; port
   `heartbeat.md` scheduled agents [9] onto it (deletes systemd-timer
   workaround).
4. **P1 — Mobile agent notifications + voice input** on the native
   Android client (plumbing exists [14]).
5. **P2 — Memory**: episodes + beliefs + pgvector; reflection workflow
   in mule; belief-diff approval cards via `ranch_ask`.
6. **P2 — Policy layer**: hooks + custom rules + auto-review agent;
   sensitivity tiers.
7. **P3 — Proactive research** (read-only tool registry) + Activity
   View.
8. **P3 — Voice loop v1** (push-to-talk call), cross-agent signals,
   specialist-dot provisioning UX.

---

## 12. Sources

Primary (read in full during this research):

1. **OpenAI — "Introducing dots."** openai.com/index/introducing-dots/
   Product definition: always-on agents, own cloud computer, learns from
   feedback, omnipresent surfaces (ChatGPT/Slack/Teams/voice), proactive
   research, specialist dots, pricing/scaling model. (Fetched via
   reader proxy; page is JS-rendered.)
2. **Earendil — "Pi Durable."** earendil.com/posts/pi-durable/ (RFC-style
   post, 2026-10-01). Harness model: conversations, tasks + checkpoints,
   ownership trees, extensions (sections/tools/hooks/tasks), compaction,
   documents, malleability, multiplayer, storage interface + conformance
   suite, ~15k LOC.

Primary (our repos, read from `~/src/`):

3. **OpenAI — "How we build safety, security, and privacy into dots."**
   openai.com/index/how-we-build-safety-security-and-privacy-into-dots/
   Auto-review, custom rules, sensitivity tiers, read-only proactive
   research enforced in code, credential isolation, monitoring/pause.
4. **forge** — `~/src/forge/README.md` (durability claim, features,
   sandbox); `docs/ARCHITECTURE.md` (message lifecycle, ToolRecorder
   split, module map); `AGENTS.md` (§1 flow, §2 sandbox rules);
   `docs/API.md` (tenancy, router, audio endpoints);
   `docs/SCHEDULED-AGENTS.md` (systemd-timer agent pattern).
5. **forge** — `crates/forge-api/migrations/007_session_embeddings.sql`
   (session summaries, REAL[] embeddings, in-app cosine, refresh
   policy); `crates/forge-api/src/embedding.rs` (Qwen3 embed/rerank via
   Bifrost, env-config).
6. **forge** — `crates/forge-api/src/` module map via
   `docs/ARCHITECTURE.md` §2 (harness event loop in api/mod.rs,
   pi_agent.rs, agent_registry.rs, tool_executor.rs, recording.rs,
   session_manager.rs 30-min cleanup, resume.rs, session_replay.rs,
   sandbox.rs, bus.rs).
7. **forge** — `docs/API.md` §"POST /v1/audio/transcriptions" (Parakeet
   STT) and §"POST /v1/audio/speech" (Kokoro TTS), `PARAKEET_URL` /
   `KOKORO_URL` env plumbing.
8. **forge/lab** — `embedding.rs` header comment referencing
   `lab/ct/embeddings/` Bifrost-served Qwen3 models.
9. **forge** — `docs/SCHEDULED-AGENTS.md` (forge-agent-setup,
   heartbeat.md, systemd timers, matrix room binding).
10. **ranch** — `~/src/ranch-2/docs/SPEC.md` (tenets, architecture,
    agent panes, agent tools incl. `ranch_ask`, triggers, webhook
    receiver); `docs/PLAN.md` (phase conventions, hot-upgrade
    constraints); `docs/design/{agent-tools,workflows-and-triggers,
    webhook-receiver,agent-builder}.md`; `mobile-native/README.md`.
11. **mule** — `~/src/mule/README.md` (primitives, stack, features).
12. **mule** — `spec.md` + `plan.md` (pi RPC migration, completed
    phases); `internal/engine/` (job processing, WASM executor,
    cancellation); `pkg/job/` (job store).
13. **ranch** — `docs/SPEC.md` §5.3, §6.2, §7 (agent tools, triggers,
    webhook relay) and `docs/PLAN.md` Phase A (spawn ownership,
    callbacks, completion detection).
14. **ranch** — `mobile-native/README.md` (foreground service holding
    the Realtime socket; background agent-event notifications;
    RN-client notification limitations it fixes).
15. **pi-acp** — `~/src/pi-acp/README.md` (ACP bridge over
    `pi --mode rpc`; session mapping; protocol-bridging precedent).
16. **forge** — `docs/SEARCH-TOOL.md` (mule-ai/search SearXNG CLI in
    the sandbox; skill-based discovery).

Secondary context (not fetched; background): OpenAI Dots system card
(deploymentsafety.openai.com/gpt-6-astra/change-log, linked from [1]);
pi-mono repo (pi RPC mode); Agent Client Protocol docs [15].
