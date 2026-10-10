# Buzz × Herd — Comparative Deep Dive

*Companion to `always-on-agents-research.md`. Research synthesis, not a plan —
build items derived from it should land as a new phase in PLAN-HERD.md if
adopted.*

**Object of study:** [block/buzz](https://github.com/block/buzz) — "a
workspace where humans and agents build together, on a relay you own."
Apache-2.0, Block, Inc. Rust monorepo (~35 crates) + Tauri/React desktop +
Flutter mobile (in development) + Postgres/Redis/S3. Cloned and read at
`326e2301` (2026-10-09).

**Our side:** the Herd initiative as it stands at the 2026-10-10 closeout
(PLAN-HERD.md status ledger: H0–H6.5 complete): ranch (surfaces), forge
(agent lives on the pi-durable harness + Postgres), mule (behavior plane:
wakes, reflection, policy, voice loop), mobile-native (client of record).

---

## 1. Buzz anatomy (what it actually is, at this commit)

### 1.1 The core idea: the relay is the workspace

Buzz is a **Nostr relay** (NIP-01 on the wire) where *every action is a
signed event*: a chat message, a reaction, a workflow step, a review
approval, a git event, an agent memory write. One `kind` integer is the only
dispatch switch; new features are new kind numbers (40000+ for Buzz custom
kinds). The relay is the single source of truth — all reads and writes flow
through it — and it enforces auth (NIP-42 WebSocket / NIP-98 HTTP),
signature verification, channel membership, persistence, Redis fan-out,
Postgres-FTS indexing, hash-chained audit, and workflow triggering
(`ARCHITECTURE.md` §4 event pipeline).

Consequences that shape everything else:

- **State is derivable.** Clients fold verified events into a local store;
  the store "should be fully derived from received events and be discarded
  as needed" (`VISION_MOBILE.md`). No opaque server state.
- **Identity is cryptographic.** Humans and agents are peers: secp256k1
  keypair, NIP-05 handle, bot role on membership. "Scoped by identity, not
  by permission flags."
- **Audit is tamper-evident by construction** — a hash-chained log
  (`buzz-audit`: SHA-256 chain over seq/ts/event/kind/actor/action,
  `pg_advisory_lock` single writer, `catch_unwind` lock release, BTreeMap
  canonical JSON).
- **Multi-tenancy = "community"** (URL-scoped workspace) — and it is
  *formally verified*: TLA+ model + Tamarin authz model, mechanically
  mutation-tested, with a runtime trace-conformance checker
  (`docs/spec/MultiTenantRelay.tla`, `crates/buzz-conformance`).
- **Open at the edges.** Nostr reads must cover all conversation state, so a
  standards-leaning Nostr client stays possible; the relay adds
  "accessories" (media, git, webhooks, derived counts) around it.

### 1.2 The agent stack

- **buzz-acp** — ACP harness: bridges relay `@mention` events → AI agents
  over stdio ACP/JSON-RPC (goose, codex, claude-code, or buzz-agent).
  Pool of 1–32 agent subprocesses, per-channel event queueing (one prompt
  in-flight per channel, batching, dedup), crash-respawn. **Does not
  persist state** — conversation continuity is a property of the relay log
  + the agent's own memory, not of the harness.
- **buzz-agent** — a purpose-built, deliberately minimal ACP agent ("read in
  an afternoon"): LLM loop, up to 8 concurrent sessions, each with its own
  MCP server child; context-full → summarize-and-continue handoff.
  **MCP-driven lifecycle hooks** (`docs/MCP_DRIVEN_HOOKS.md`): tools named
  `_*` (e.g. `_Stop`, `_PostCompact`) are invisible to the LLM, called at
  lifecycle points, responses injected as tool results — a clean hook
  convention worth stealing (see §5.4).
- **buzz-dev-mcp** — shell + str_replace + todo + rg/tree for any ACP agent;
  process-group kill, bounded output, zero-unsafe.
- **buzz-cli** — agent-first CLI, JSON in / JSON out, mirroring the full
  platform surface (channels, DMs, messages, reactions, feed, workflows,
  repos/projects/PRs/patches, uploads, canvases, moderation, `mem`,
  agents…). "Agents can script the entire platform without a GUI."
- **NIP-98** — every HTTP call carries a Schnorr-signed `kind:27235` event
  bound to URL+method, ±60 s window, replay set. **NIP-OA**
  (`docs/nips/NIP-OA.md`) — owner *attestation*: `["auth", owner,
  conditions, sig]` signed over the agent pubkey + conditions; a reusable
  capability credential that proves the owner authorized *this* agent under
  *these* conditions, without the event ever becoming owner-authored.
- **Personas & teams** — a persona = model + system prompt bundle; a team =
  named group of personas ("Ralph for review, Scout for research").
  Desktop-managed (`crates/buzz-persona`).

### 1.3 Agent memory: engrams (NIP-AE)

Agent memory is **signed, encrypted Nostr events** (`kind:30174`,
`docs/nips/NIP-AE.md`): addressable slugs — `core` (identity/rules/goals,
exactly one) and `mem/…` (records) — NIP-44-encrypted with the symmetric
agent↔owner conversation key, so **the owner can always read everything the
agent remembers** but no one else can. Slug addressing is HMAC-derived
(slugs never appear in plaintext tags); writes are monotonic
(`created_at = max(now, head+1)`), tombstoned, and best-effort conflict
checked; `[[slug]]` wiki-links give a reachability graph rooted at `core`,
with orphans surfaced for human review (never auto-deleted). The desktop
ships a memory graph UI (`desktop/src/features/agent-memory/`); `buzz-cli
mem` writes/reads them.

Note what's *not* there: no embeddings, no vectors, no confidence/decay, no
automatic reflection. Memory is **deliberate, human-auditable recordkeeping**
by the agent (via CLI/tools), retrieved by its own reading. The system
"learns" to the extent the agent writes good engrams.

### 1.4 Workflows

Channel-scoped **YAML-as-code** automation in `buzz-workflow`: 4 triggers
(`message_posted`, `reaction_added`, `schedule` (real cron), `webhook`), 7
actions (`send_message`, `send_dm`, `set_channel_topic`, `add_reaction`,
`call_webhook` (SSRF-guarded), `request_approval`, `delay`), `evalexpr`
conditions (100 ms timeout), template vars, per-run traces in Postgres,
100-permit concurrency. **Known gaps (self-declared, `ARCHITECTURE.md` §9):**
approval gates exist end-to-end except the executor — a run hitting
`request_approval` is marked Failed (WF-08); `send_dm`/`set_channel_topic`
unimplemented (WF-07). No LLM steps, no WASM steps, no agent step: a
workflow posts messages/calls webhooks, it does not *think*.

### 1.5 Huddles (group voice)

Real-time voice is a **WebSocket Opus relay inside the relay**
(`buzz-relay/src/audio/`): NIP-42 auth per participant, channel membership
admission, in-memory room (soft cap 25), 8-byte header (seq, 48 kHz
timestamp, level, flags) + opaque Opus frames, lifecycle as Nostr events
(started/joined/left/ended). No external SFU; recording per-track
unimplemented. The *desktop* carries the audio AI: STT auto-transcription
of huddle audio (`huddle/stt.rs`, `transcription.rs`), and an **agent TTS
publisher** — when a managed agent posts a message in the live huddle
channel, the desktop synthesizes TTS (local "Pocket" models via
`crates/buzz-voice`, or external) and streams the Opus into the huddle as
another participant (`huddle/agent_tts_publisher.rs`,
`agent_tts_routing.rs`). Plus barge-in, speaker cancellation, jitter
buffers, "human floor." So agents "bring their own STT/TTS" in the sense
that the *client that manages the agent* provides both, and the agent's
channel messages become its voice.

### 1.6 Push, mesh, remote agents, git

- **Push** (`crates/buzz-push-gateway`): APNs-only (Apple), App-Attest
  enrollment, Postgres-stored grants/leases, "experimental"
  (`docs/buzz-push.md`). No FCM/Android.
- **Buzz Mesh**: community GPU compute pooling over **iroh + mesh-llm**
  (pinned `v0.76.2`): members opt in, the relay gates discovery/trust with
  channel membership (member status as relay events), agents consume a
  local OpenAI-compatible endpoint; models too big for one machine split
  across several. Separate `buzz-relay-mesh` (inter-relay QUIC mesh for
  multi-node relays) and desktop `mesh_llm` coordinator.
- **Remote agents** (`docs/remote-agents.md`, `VISION_REMOTE_AGENTS.md`,
  `crates/buzz-backend-kubernetes`): the agent's identity/history/presence
  live on the relay, so the *body* is replaceable. Deploy via a **provider
  binary contract** (K8s first): one-way handoff, no substrate control
  channel after deploy — steering, status, shutdown all flow over the
  relay; a conformance suite pins provider behavior; agents
  **self-reap** (inactivity timer; "compute is rented by attention").
- **Git**: relay-hosted repos, standard smart HTTP, pushes signed with the
  user's/agent's npub (`git-sign-nostr`/`git-credential-nostr`); branches
  become channels; merge = archived channel record.

### 1.7 The security crown jewel: the IFC broker (draft)

`docs/practical-information-flow-for-buzz-agents.md` (draft, Jordan Mecom)
is the most rigorous agent-security design I have seen in an OSS repo: a
**trusted broker holds the agent's signing key** and mediates all Buzz
reads/writes/memory for **audience-bound, keyless agent instances**, with
reader-set confidentiality labels formalized as a lattice
(Denning/Myers–Liskov), noninterference claims over the broker-mediated
path, membership *epochs* (roster change ⇒ fresh session, revoked
authorization), and human-approved **declassification** grants
(exact content, exact destination, single-use). The label algebra is
already factored into a generic, Buzz-free crate: **`ifc-core`**
(confidentiality labels + `FlowState` egress checks). The doc is explicit
about what it does *not* cover (shell/files/network laundering) and how
CaMeL/FIDES complement it. **Status: design + label crate; the broker
itself is not built.**

### 1.8 Declared status & gaps (honest from the repo)

✅ relay/auth/pubsub/search/audit, MCP surface, ACP harness, desktop app,
channel features, workflow engine, identity, CLI, personas, huddles (voice
+ lifecycle), mesh. 🚧 approval-gate executor (WF-08), mobile (Flutter,
in progress). 📋 remote agents (K8s, spec in review), culture features.
Known-limitations table (§9) also admits: **no rate limiting implemented**,
no huddle recording, sqlx runtime queries.

---

## 2. Herd recap (state at the 2026-10-10 closeout)

From `PLAN-HERD.md` (status ledger; all phases H0–H6.5 ✅):

- **H0/H2 — durable agent lives.** Forge's core is now a Node harness
  running **pi-durable upstream** against a **Postgres storage backend**
  (H0.2 conformance 23/23, kill-9 green; H2.0–H2.6 deleted
  `resume.rs`/`session_replay.rs` and the legacy turn driver). Durable
  tasks/checkpoints, ownership trees + subagents, durable timers (atomic
  fire-claim), compaction-as-task, documents, prompt sections,
  `before_tool` hooks — one writer, one truth.
- **H1 — the Agent entity.** `agents` table in forge (name, avatar, owner,
  home_machine, primary_profile, visibility, memory_scope, credentials
  scope); ranch frames `AgentList`/`AgentTalk` (continue/new/fork); mobile
  registry screens; "talk to agent anywhere".
- **H3 — mule as behavior plane.** Forge-grade tenancy; `forge_conversation`
  step (addressing into agent conversations, incl. fork mode); **wakes**
  (cron/webhook/event/job live; memory/agent_signal kinds) with
  rate-limit guardrail + wake_log; per-job WS subscriptions; **policy
  engine** — deterministic allow/deny/ask rules, action-hash memoization
  (never re-ask), `before_tool` hook in forge wired to mule, `ask` ⇒
  `ranch_ask`-style approval card with fail-closed on timeout.
- **H4 — memory & learning.** Episodes + beliefs in Postgres/pgvector
  (2560-dim Qwen3 embeddings), turn-end episode capture (deterministic
  extractor + redaction + provenance), `memory:beliefs` prompt section,
  **reflection as a mule workflow** (WASM pull/check/submit: dedupe,
  contradiction, decay; agent step proposes belief diffs), belief-approval
  cards (Keep/Forget/Edit), memory watch-triggers ⇒ wakes, cross-agent
  `agent_signal` with delegator-scoped episode access.
- **H5 — proactivity.** Proactive research as durable tasks with a
  **structurally read-only tool registry** ({read, search, webfetch, note});
  wake matrix complete (webhook/event/cron/file-watch/turn-end/reminder);
  **Activity View** — daemon poller worker, `ActivityPush` frames, 8 item
  kinds across agents + machine-local work, pause kill switch
  (`AgentPause`, `ranch pause|unpause`), TUI + both mobile clients.
- **H6 — presence & voice.** Agent-scoped **mobile notifications with
  action buttons** (approval/research/memory/task_done lanes, per-agent
  mute/priority, buttons work locked/backgrounded over the
  foreground-service Realtime socket); voice input (Parakeet
  push-to-talk + on-device recognizer); **voice loop v1+v2** — mule
  `voice_call` step, single-loop state machine, ratio endpointing,
  barge-in with <150 ms stop-to-flush + client echo guard; specialist
  provisioning (`ranch agents new` wizard + mobile ＋Agent), secrets
  store, `web_login` cookie broker.

Transport/substrate: `ranchd` per machine, JSON-lines frame protocol shared
by unix socket and Supabase Realtime relay; forge = OpenAI-compatible API
over Postgres; mule = OpenAI-compatible workflow API over Postgres + WASM
+ agent steps. Posture: self-hosted, own your infra.

---

## 3. Where we converged — the same ideas, built independently

The striking part of this comparison: two teams, different sponsors, no
shared code, and roughly the same **vocabulary of agent life**.

| Capability | Buzz | Herd | Verdict |
|---|---|---|---|
| **Agent ≠ process/surface.** A durable identity with many conversations and many surfaces | Agent = Nostr keypair + profile + memberships + audit trail; relay holds the history, machine is "just where it works" | Agent = forge `agents` row + home_machine + memory/credentials scope; one agent, many conversations (pi-durable forks), many panes/surfaces | Same concept (this is the Dots thesis; we both implement it). Buzz scopes it to a *team workspace*, ours to an *owner's herd*. |
| **Always-on wake machinery** | Workflow triggers: message/reaction/schedule/webhook + cron engine in the relay | mule `wakes`: cron/webhook/event/job/memory-trigger/agent-signal + ranch trigger scheduler (machine-local edge) + forge durable timers + file-watch + turn-end forwarders | Ours is broader (memory triggers, turn-ends, machine-local triggers, per-wake rate guardrail); theirs adds channel-message/reaction triggers in a shared room. |
| **Approval-gated consequential actions** | `request_approval` workflow step: schema, REST, MCP tool, UI — **executor not wired (WF-08)** | H3.5 end-to-end: mule policy engine ⇒ forge `before_tool` hook ⇒ approval card on every surface ⇒ memo so it's never re-asked; fail-closed | Ours is the working system; theirs is the scaffold. |
| **Custom rules / policy plane outside the agent** | The IFC broker design (draft): key-holding broker, audience-bound keyless instances, lattice labels, declassification grants | mule as policy plane (research §2.2's design lesson): rule store + evaluator + hooks, human-only rule mutation, `ranch_ask` gates | Same design lesson (enforcement lives outside the agent). Buzz has the *formal model* (lattice, noninterference, epochs) and a label crate; we have the *implemented pipeline* but a coarser model (deterministic rules + owner tenancy). |
| **Agent memory the owner can audit** | Engrams: signed NIP-44-encrypted records, owner-readable, slug-addressed, wiki-linked, orphans reviewed by a human, never auto-deleted | Episodes/beliefs: embeddings, confidence, decay, provenance, versioning, belief-approval cards, `belief_audit` rows | Same principle, different substance: theirs = deliberate signed record; ours = learned, retrieved, confidence-weighted state. (More in §4.4.) |
| **Visible delegated work — the Activity View** | `VISION_ACTIVITY.md`: "verb, object, outcome"; 12 render classes; "never go dark"; "failures rise; reads recede"; raw rail as safety net | H5.3: cross-agent timeline (8 item kinds: tasks, pending beliefs, open research, wakes, asks, spawns, triggers, mule jobs), deep-links, pause, on TUI + both phones | Same product goal (supervise delegation at a glance, "earn delegation"). Theirs is a *design doctrine* with a render taxonomy; ours is *shipped but flatter* (kind + text rows). Their doc is the best spec of our v2. |
| **Proactive, read-only background work** | No first-class equivalent; agents are scoped by channel identity | H5.1: durable research tasks whose tool registry *structurally* lacks write tools; reports surface as suggestion cards, never auto-actions | Ours (directly from Dots [1][3]). |
| **Voice with agents** | **Group huddles**: Opus WS relay in the relay; agents join by membership; desktop does STT of the room + TTS of agent messages; barge-in/speaker-cancellation; lifecycle as events | **1:1 calls**: mule `voice_call` state machine, full-duplex barge-in (<150 ms flush), echo guard, STT via Parakeet / TTS via Kokoro proxies, agent steered mid-turn | Different shapes of the same idea (buzz = meeting room, us = phone call). Buzz's huddle is the more social object; ours is the lower-latency, more instrumented object. |
| **Mobile presence in the background** | Flutter app, offline-first, "don't trust relays", push via APNs gateway (experimental, Apple-only) | Native Kotlin client of record: foreground service holds the Realtime socket; agent notifications **with action buttons that work locked** (approval/research/memory); per-agent mute/priority | Ours is further along on the Android we actually target; their *principles* (offline-first, derived store, don't lose drafts) are a better spec than anything we have for TUI/web cache behavior. |
| **Self-hosted, open, sovereign** | `VISION_SOVEREIGN.md`: run your own relay, one community = one workspace, same OSS for hosted multi-tenant | AGENTS.md posture: "open source, self-hosted, own your own infra"; Supabase relay with documented self-host option | Same posture, different cloud coupling (buzz: zero external deps — Postgres/Redis/S3; us: Supabase Realtime in the relay path). |
| **Kill switches** | Presence lease + agent self-reap (idle timer) | `AgentPause` per-agent + whole-herd pause (admission 409, timers no-op, wakes bulk-paused) | Complementary: theirs bounds *cost* (don't run when nobody's there), ours bounds *anxiety* (stop the agent now, everywhere, from your phone). |
| **Malleable agent behavior** | Personas (model+prompt bundles), teams, MCP hook convention (`_*` tools at lifecycle points), relay registry hot-swap | pi-durable extensions (prompt sections/tools/hooks, installed by name, hot-swappable), profiles, skills, web_login/credentials scope | Same thesis (behavior is data, swapped by name at the boundary); buzz's `_Stop`/`_PostCompact` hook convention is a cheaper trick than our before_tool chain for harness-level lifecycle control. |

**The one-liner:** buzz is the **multi-user team workspace** version of
Herd — signed event log, rooms, channels, git, group voice, compute
commons — and Herd is the **single-owner always-on household** version: a
terminal-multiplexer presence layer over a durable agent life with real
learning, real policy enforcement, and a working voice loop. Both are
"Dots, self-hosted"; we split the design space along *who the agents serve*
(team vs. owner) and *what the source of truth is* (event log vs.
relational rows).

---

## 4. Where we diverge — and what the divergence buys

### 4.1 Source of truth: event log vs. relational rows

Buzz: **append-only signed event log** (Nostr events in Postgres, monthly
partitioned). Truth is the log; everything else (channels, read state,
feeds) is derived. Idempotent inserts (`ON CONFLICT DO NOTHING`), any
client can re-derive state, the audit trail is hash-chained and
client-verifiable, identity is keypair-derived and portable across any
Nostr client. Cost: a log is awkward for transactions and for *app state*
(canvases, documents, workflows are bolted on as separate Postgres rows —
the log covers conversation and audit, not everything).

Ours: **relational state** in two Postgres databases (forge: messages,
agents, tasks, memory, secrets; mule: workflows, jobs, wakes, policies)
plus a **custom frame protocol** to surfaces (unix socket + Supabase
Realtime). Truth is the rows; clients are live subscribers, not log
folders. Cost: no cryptographic identity, no client-verifiable audit,
frame protocol is ours-only (Supabase relay couples the MVP cloud path).
Benefit: real transactions, real SQL, pgvector, LISTEN/NOTIFY semantics,
and a surface protocol that can carry a *terminal* (65 KB of VT grid
frames) — something an event log was never shaped to hold.

**Judgment:** for a one-owner, machine-anchored system, relational rows +
private relay is the right call (it is what we chose; Supabase coupling
is the accepted risk, research §10.5). Buzz's model only pays off at the
scale where *many people and many clients* need one verifiable room. But
two elements of the log model are stealable without adopting Nostr: the
hash-chained audit (§5.1) and "client state is derived, not authoritative"
(their mobile doctrine, §6.2).

### 4.2 Agent runtime: ACP harness vs. pi-durable

Buzz's agent stack is **ACP + MCP**: `buzz-acp` (relay → stdio ACP →
goose/codex/claude or buzz-agent), `buzz-agent` (minimal LLM loop, MCP
tools, self-compacting handoff), `buzz-dev-mcp` (shell/edit). The harness
**does not persist state** — durability is the relay log + engrams; a dead
turn is a dead turn (the agent process respawns; its per-channel session
re-seeds from context the agent assembles).

Ours is **pi-durable upstream** in a Node harness: every step of a turn is
a checkpointed durable task; kill-9 mid-turn recovers exactly-once;
timers fire exactly-once; subagents own their child tasks; compaction is a
background task. This is *strictly stronger mid-turn durability* — it is
the H2 refactor's whole point.

But note the asymmetry buzz draws: their **remote-agent model** makes the
*body disposable* (identity/history/presence on the relay; substrate is a
detail; self-reaping) — a property we don't have yet, since a dot's home
is one forge machine. Their `VISION_REMOTE_AGENTS.md` + provider contract
is the cleanest statement of "agent home is the control plane, not the
host" I have seen (§5.5).

Also: buzz-agent's **MCP hook convention** (`_Stop` = object if the LLM
finishes with open todos; `_PostCompact` = re-inject context after
handoff) is a harness-level lifecycle hook that costs one tool-name prefix
— cheaper and more expressive at the harness edge than our before_tool
chain for lifecycle control (can't block a tool; can veto *turn end*).
Adoptable in our harness without touching pi-durable (§5.4).

### 4.3 Multi-user rooms vs. owner-private herd

Buzz's fundamental unit is the **channel**: humans @mention an agent in a
shared room; the agent replies there; everyone's context is the room.
Access is channel membership; an agent's authority is exactly its
memberships. This gives multi-user collaboration for free (reviews,
triage, incident channels) and makes "agents as teammates" literal.

Our fundamental unit is the **agent's conversation with its owner** (+
subagents + cross-agent signals). Rooms don't exist; a forge agent
belongs to one user; "org" memory scopes and agent signals are the
multi-agent glue. This gives us 1:1 trust semantics and a phone-centric
presence story; it forecloses the team-workspace use case (and
conversely, buzz forecloses "watch your agent's terminal and its whole
machine").

This is the *product* split, not an engineering gap. Neither should
"fix" itself toward the other; but the IFC doc shows exactly what the
team side needs to keep an agent's private context out of a public room —
and our `memory_acl`/signals are the owner-side analogue (§5.3).

### 4.4 Memory: learned beliefs vs. signed records

Our H4 is a **learning system**: automatic episode capture, embeddings,
reflection workflows, confidence/decay/provenance, belief-approval cards,
vector retrieval into prompts. It optimizes for *the agent getting better
without being asked*.

Buzz's engrams are a **record system**: the agent writes what it decides
to remember, in readable text, encrypted to the owner, with explicit
links and orphan review. It optimizes for *the owner being able to read
and audit everything the agent knows* — a guarantee our system does not
make (a belief's provenance traces to episodes, but nothing is
cryptographically bound or owner-readable out-of-band).

These compose rather than compete: engrams are the *export/audit layer* a
learning system lacks; beliefs are the *retrieval layer* engrams lack.
If we ever interop or export ("show me everything Iris knows, signed"),
NIP-AE is the natural target shape (§5.2). One concrete borrow regardless:
their **tombstone + orphan-review** discipline (never auto-delete memory;
surface unreachable records for human review) maps directly onto our
`beliefs(forgotten)` lifecycle and is worth codifying as an invariant.

### 4.5 Voice: huddle vs. call

Buzz: **multi-party room** (Opus relay, agents join by channel
membership, STT of the room, TTS of agent posts, lifecycle as events;
recording not built). Social object: a meeting where the agent is a
participant. Latency budget unknown (TTS via desktop pipeline).

Ours: **two-party call** (mule `voice_call`: single-loop state machine,
energy/ratio endpointing, barge-in with measured <150 ms stop-to-flush,
out-queue liveness, client echo guard, measured STT/TTS latency budgets
against live Parakeet/Kokoro). Technical object: a phone call, with
numbers.

Buzz has what we lack: a *group* room and the "agent speaks its channel
messages" pattern. We have what buzz lacks: instrumented latency,
barge-in semantics, and a loop that steers a *durable* agent mid-turn.
A group huddle over a herd would be a genuinely new pane type (§6.1).

### 4.6 Compute: commons vs. lab

Buzz Mesh pools **members' idle GPUs** (iroh/mesh-llm, relay-gated
discovery, models sharded across machines; "the community is your
compute"). Our infra is a **lab fleet** (ALC containers, dedicated GPU
host for STT/TTS/embeddings) — right for one owner who operates the
machines, wrong for a community where you don't control the hosts.

If herd dots ever span machines we don't centrally operate (e.g. each
family member's box), buzz's mesh pattern — and mesh-llm upstream, exactly
like we use pi-durable upstream — is the design to copy rather than
rebuild (§5.6).

### 4.7 Security posture: formalized tenancy vs. operational depth

Buzz: multi-tenant isolation **formally modeled** (TLA+ + Tamarin,
mutation-tested, runtime trace conformance) + tamper-evident hash-chained
audit + NIP-OA owner attestation + a (draft) IFC lattice for agent
information flow. Weak where the ledger admits: no rate limiting,
approval gates not wired.

Ours: operational depth in the *enforcement path* — fail-closed policy
evaluation, memoized decisions, cookie-never-in-context web_login (grep
audited), structural read-only research registries, restricted-key
gates everywhere. But tenancy is conventional (Postgres rows + API keys,
owner-scoped queries), audit is per-domain tables (`belief_audit`,
`policy_audit`, wake_log) rather than one tamper-evident chain, and there
is no formal model.

The IFC doc + `ifc-core` is the most valuable security artifact in the
whole repo for us: our agents already carry cross-audience data
(org memory, agent signals, episodes visible to subagents) with ACL rows
as the whole story; a reader-set label lattice would make those flows
*provable* instead of *queried* (§5.3).

### 4.8 Terminals: they don't have ours

Buzz agents get a *shell tool* (buzz-dev-mcp) inside their body. We have
**server-side terminal emulation** — the agent's whole machine is a pane
any surface can watch, key-encoded through ghostty-vt, with hot-upgrade
fd inheritance. No analogue exists in buzz; "inspect the agent's
computer" is, for us, literally watching the terminal it lives in.
Nothing to replace; this is an asset to keep (research §4.3's
"inspectable" gap is closed for us in a way buzz's model doesn't even
attempt).

---

## 5. What we could replace with buzz code

Honest verdict up front: **almost nothing we've built is better built by
buzz.** Where we overlap (wake machinery, approval cards, memory, policy,
voice loop, surfaces), ours is either more complete (approval gating,
memory/learning, voice latency) or more durable (pi-durable harness vs.
stateless ACP harness), or shaped differently (frames/Postgres vs.
Nostr/Redis). The things buzz has that we lack are *extensions*, not
substitutes — with two exceptions that are genuine "replace what we
hand-rolled" wins:

### 5.1 Adopt: hash-chained audit (`buzz-audit`) → replace our
per-domain audit tables

We have *no* tamper-evident audit; `belief_audit`, `policy_audit`,
`wake_log` are ordinary Postgres tables. `buzz-audit` is small, clean,
Apache-2.0, and generic enough to re-target: SHA-256 chain (seq, ts,
event-ref, kind, actor, action, canonical-metadata JSON),
`pg_advisory_lock` single-writer, panic-safe lock release,
`verify_chain()` walker.
**Plan:** one forge `audit_log` chain covering agent-acting events
(turn end, tool call intent + result, memory writes, policy verdicts,
pause/resume, web_login, provisioning), with the verifier runnable from
`ranch` CLI/TUI ("prove me the audit is intact"). ~1 crate port; the
action enum is ours to define. This is the single highest-value code
adoption.

### 5.2 Adopt: NIP-AE engrams as an *export/interop shape* (not storage)

Do not replace pgvector beliefs — retrieval and learning live there. But
adopt the NIP-AE conventions as the shape of an **`engram export`**:
agent memories rendered as signed, NIP-44-owner-encrypted records with
`core` + `mem/…` slugs and `[[links]]`, published (or at least emitted to
disk) so the owner can read everything the agent knows, off-system, with
integrity. Also adopt their **lifecycle discipline** as invariants:
tombstones instead of deletes, orphan review surfaced to the human, no
auto-deletion of memory. Cheap: an exporter over existing
`beliefs`/`episodes` + key material per agent.

### 5.3 Adopt: `ifc-core` labels → upgrade `memory_acl`/signals to a
label lattice

`ifc-core` is deliberately Buzz/Nostr-free: reader-set confidentiality
labels, join = intersection, egress checks, `FlowState` per domain.
Mapping: beliefs/episodes/signals get `R(label)` reader sets (owner,
agent, subagent-scope, org) derived at write time; cross-agent episode
access and org-signal delivery become egress checks
(`A(d) ⊆ R(z)`) instead of ACL-row queries; agent instances (subagent
conversations) become "domains" that can't move data to a wider audience.
This directly operationalizes research §10.4's prompt-injection-at-fleet-
scale concern ("one compromised specialist dot with org-memory write
access is a lattice to pivot through") with a formal model instead of
trust us. Effort: crate is small; the work is threading labels through
forge memory + mule reflection writes.

### 5.4 Adopt: `_*` lifecycle-hook convention → harness edge

Port buzz-agent's `_Stop` / `_PostCompact` trick into our forge harness
(one extension, tools named `_…`, called at turn-end/compaction, response
injected as tool result, invisible to the LLM). Use cases: todo
enforcement before `end_turn`, re-injecting belief/section context after
compaction handoff, "did you file the report?" gates. Complements (does
not replace) the mule policy `before_tool` chain. Tiny: an extension + a
few harness call-sites.

### 5.5 Adopt: remote-agent provider contract → "move this dot"

Copy the *design* (and possibly `buzz-backend-kubernetes` code) of
"same agent, new body": one-way deploy handoff, no substrate control
channel afterward, steering/status/shutdown over forge/ranch, self-reaping
idle timer ("compute is rented by attention" — we currently have
`AgentPause` but no auto-suspend; an idle-timer that pauses + reaps a
dot's harness subprocess while its identity/memory/wakes stay registered
is a direct, cheap win), provider conformance tests. This turns
`agents.home_machine` (an H1 field that's currently mostly a label) into
a live migration primitive.

### 5.6 Use upstream: mesh-llm (iroh) → herd compute commons

If (not when?) dots live on machines we don't centrally operate, use
**mesh-llm upstream** exactly as we use pi-durable upstream: a
`Share compute` toggle in ranchd that pools this machine's GPU into the
herd, discovery gated by the ranch registry, OpenAI-compatible local
endpoint per agent. Do *not* re-implement iroh/mesh plumbing. Gated on
need — the lab fleet covers single-owner compute today (§4.6).

### 5.7 Do NOT replace (explicit non-adoptions)

- **The relay.** Do not swap ranchd's frame protocol/Supabase relay for a
  Nostr relay. Our model is one owner + machine-local first + terminal
  frames; an event-log relay is a worse fit and a 3× ops surface
  (Postgres+Redis+S3 vs. our current footprint). Keep the accepted-risk
  posture (research §10.5) and the documented self-host option.
- **The agent runtime.** buzz-agent/buzz-acp under the herd would
  *downgrade* durability (no checkpoints, no exactly-once timers). Keep
  pi-durable (H0.3 decision stands; buzz is no counter-evidence — if
  anything it validates the "small auditable harness" ethos we already
  bought by pinning pi-durable upstream).
- **The workflow engine.** mule (agent steps, WASM, job queue, OpenAI-
  compat API, wakes, policy) strictly exceeds `buzz-workflow`
  (message actions, unwired approvals).
- **Voice loop.** Our mule `voice_call` is the instrumented, working
  object; buzz's is desktop-coupled. (Their *room* is different, §6.1.)
- **APNs push gateway.** We're Android-only by posture; if iOS ever
  lands, study `buzz-push-gateway` (App-Attest + grant/lease model) as
  reference, not import.

---

## 6. Extensions we can add to ranch/herd leveraging buzz

Prioritized (value ÷ effort), each a candidate **H7** work item — none
blocks the current closeout:

1. **H7.1 Audit chain** (§5.1). Tamper-evident, client-verifiable agent
   action log + `ranch audit verify`. The trust story for an always-on
   herd, and the thing a skeptical owner asks for first.
2. **H7.2 Activity View v2** — adopt `VISION_ACTIVITY.md` as the spec:
   verb/object/outcome cards, the 12 render classes mapped onto our 8
   item kinds, "never go dark" (idle/timeout as rendered states — we
   currently show items only while they're *in* the set), "failures
   rise; reads recede" (error items jump the sort, reads/reasoning
   demoted), raw rail toggle on TUI, progressive disclosure on mobile.
   Spec is written; it's rendering work on top of the shipped
   `ActivityPush` frames.
3. **H7.3 Label lattice for memory** (§5.3) — `ifc-core` in forge;
   egress-checked cross-agent flows; declassification as an explicit
   owner grant (one card, exact content, exact destination, single-use)
   instead of org-wide `memory_acl` toggles.
4. **H7.4 Huddle pane** — group voice over the herd: humans + several
   agents in one room. Reuse buzz's relay-embedded Opus room pattern
   (8-byte header frames, admission, lifecycle events) as a mule `voice`
   generalization (N in-sockets, one agent TTS publisher per managed
   dot, mule keeps VAD/barge-in/endpointing). This is the "family + herd
   in the kitchen" object our 1:1 calls aren't.
5. **H7.5 Permission-aware search over agent life** — buzz-search's
   pattern (generated `tsvector` column + GIN on insert; privacy kinds
   storage-level `NULL`-ed; caller re-authorizes hits) applied to forge
   `messages`/`durable_entries`, episodes, beliefs: one Cmd-K "find that
   thing Iris said / the invoice agent did in March" across the herd, on
   every surface.
6. **H7.6 Dot migration + idle self-reap** (§5.5) — provider contract
   over `home_machine`, one-way deploy, forge-anchored identity,
   idle-timer pause/reap with identity retained.
7. **H7.7 Engram export** (§5.2) — signed, owner-encrypted memory
   export; orphan-review UI pass (surface unreachable/forgotten beliefs
   for human disposition).
8. **H7.8 (stretch) ACP lane** — run non-pi agents (goose/codex/claude)
   *alongside* dots via buzz-acp-style harnessing, for owners who want a
   second runtime. Lowest priority; conflicts with the H0.3 pin
   philosophy, keep behind a flag.

Out of scope by posture: multi-user rooms/channels (§4.3), git hosting,
canvases-as-product, culture features, Nostr interop beyond export.

---

## 7. Risks of coupling to buzz

- **Churn.** Buzz is young (v0.1.0 everywhere, mobile in flight, known
  gaps table). Treat everything as *reference + selective import*, never
  as a dependency; pin like we pin pi-durable. The import-safe set is
  small and generic: `ifc-core`, `buzz-audit` patterns, mesh-llm,
  provider-contract design, NIP-AE constants, `_hook` convention.
- **License.** Apache-2.0 — compatible, attribution-only. No issue.
- **Nostr gravity.** Each Nostr-shaped import pulls identity toward
  keypairs. Keep Nostr *export-only* unless/interoperability is a real
  requirement; the herd's identity stays forge rows + ranch frames.
- **Their gaps are load-bearing.** Approval gating (WF-08) and rate
  limiting are unimplemented at this commit; any "buzz does X" claim
  above is marked by status for that reason. Re-verify before importing.
- **Our scope discipline** (AGENTS.md tenets: multiplexer first; every
  new worker obeys the hot-upgrade constraint) applies unchanged to any
  H7 worker (huddle room, search index, audit verifier) — they are
  additive pane types + workers, the multiplexer core must not regress.

## 8. Reference map

Buzz (paths in the clone at `326e2301`):

- `README.md`, `ARCHITECTURE.md` — crate map, event pipeline, known
  limitations (§9)
- `VISION.md`, `VISION_AGENT.md`, `VISION_ACTIVITY.md`,
  `VISION_MESH.md`, `VISION_MOBILE.md`, `VISION_REMOTE_AGENTS.md`,
  `VISION_PROJECTS.md`, `VISION_SOVEREIGN.md`
- `docs/practical-information-flow-for-buzz-agents.md` — IFC broker
  design (draft): lattice, epochs, declassification, noninterference
- `docs/nips/NIP-AE.md` — engrams (kind 30174), NIP-OA — owner
  attestation
- `docs/MCP_DRIVEN_HOOKS.md` — `_*` lifecycle hooks; `docs/buzz-push.md`
  — APNs-only push; `docs/remote-agents.md` — provider contract
- `crates/buzz-audit` — hash chain; `crates/ifc-core` — label algebra;
  `crates/buzz-search` — FTS pattern; `crates/buzz-workflow` — YAML
  engine; `crates/buzz-acp` — relay→ACP bridge; `crates/buzz-agent` +
  `crates/buzz-dev-mcp` — the minimal agent; `crates/buzz-cli` — agent
  surface (incl. `mem`); `crates/buzz-push-gateway` — APNs;
  `crates/buzz-backend-kubernetes` — substrate provider;
  `desktop/src-tauri/src/huddle/` — voice room + agent TTS;
  `desktop/src-tauri/src/mesh_llm/` — mesh coordinator

Herd (this repo + siblings, at 2026-10-10 closeout):

- `docs/PLAN-HERD.md` — status ledger H0–H6.5 + closeout (acceptances,
  deferrals, H0.3 kill criteria)
- `docs/research/always-on-agents-research.md` — the concept this doc
  compares against
- forge: `agents` (016), harness/ (pi-durable + durable-pg),
  timers/subagents/compaction/docs/hooks (H2.x), memory 022–024 +
  `memory_capture.rs` + reflection cards (H4.x), `agent_research`
  (025), `agents_paused` (026), secrets + web_login (027), policy
  `before_tool` hook (H3.5)
- mule: `wakes` (0014), `policies` (0015), `forge_conversation` step,
  `internal/voice/` (H6.3/H6.4), `examples/reflection/` (H4.4)
- ranch-2: agent registry + Activity View workers/frames (H1.3/H5.3),
  notification policy + actions (H6.1), `voicecall.rs` + CallActivity
  (H6.3), `agents_new.rs` + AgentProvision (H6.5)

*Sources for the Dots/pi-durable claims: research doc §12 [1]–[3].*
