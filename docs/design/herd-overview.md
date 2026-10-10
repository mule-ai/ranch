# Design: The Herd — concept architecture + sequence diagrams

*Multiple always-on agents per user ("dots in the herd") that act
proactively, learn from themselves and each other, and are always
visible and interruptible. This is the concept view of PLAN-HERD
(phases H0–H6); it describes the SYSTEM, not the implementation —
file/line detail lives in PLAN-HERD and the per-repo docs.*

Status: shipped (H0–H6.5 complete, 2026-10-10). Companion docs:
research § concept (`docs/research/always-on-agents-research.md`),
wire frames (`docs/PROTOCOL.md` §8.6–8.7), target spec
(`docs/SPEC.md` §5.5–5.8).

## 1. Division of labor

The one-paragraph version (from this repo's AGENTS.md):

- **forge hosts the agents' lives** — the `agents` entity
  (distinct from sessions/conversations and from ranch panes),
  the durable runtime (pi-durable on Postgres: durable tasks,
  checkpoints, ownership trees, hooks, documents, compaction),
  episode/belief memory (pgvector), proactive research, and the
  web-sign-in broker. One agent, many conversations, many
  surfaces.
- **mule scripts their behavior** — the behavior plane: wakes
  (cron/webhook/event/job/memory-trigger), the policy engine
  (rules + auto-review that constrain agents from OUTSIDE, via
  forge's before_tool hook), the reflection workflow (learning
  loop), and the voice loop (call state machine).
- **ranch is everywhere they can be seen and heard** — the
  agent registry on every surface ("talk to an agent from any
  surface"), the Activity View (background/proactive work),
  agent-scoped mobile notifications with actions, voice input
  and the `ranch call` / phone call screen.

```
                    ┌──────────────────────────────────────────┐
                    │                 the user                 │
                    │  TUI · web · Android (native + Expo)     │
                    └───────┬──────────────┬──────────────┬────┘
                            │ frames       │ frames       │ WebSockets
                            ▼              ▼              ▼
   ┌─────────────────────── ranchd ────────────────────────────────┐
   │ registry (AgentList/Talk/Busy) · Activity poller (H5.3)       │
   │ card lanes: policy_ask · memory_review · research_report ·    │
   │   web_login (AgentAsk round-trips posted to /ranch-tools)     │
   │ `ranch call` (H6.3) → mule jobs API → two WebSockets         │
   └───────┬───────────────────────────┬───────────────────────────┘
           │ forge API (localhost)     │ mule API
   ┌───────▼────────────── forge ──────┴──────────┐  ┌───────────── mule ──────────────┐
   │ agents · durable runtime (pi-durable/PG)     │  │ wakes (cron/webhook/event/      │
   │ memory: episodes/beliefs/signals (pgvector)  │◄─┤   job/memory-trigger)           │
   │ research (read-only by construction)         │  │ policy engine (evaluate/memo/   │
   │ web-login broker · secrets                   │  │   proposals/audit)             │
   │ before_tool hook ────────────────────────────┼──┤ reflection workflow (H4.4)      │
   │                                              │  │ voice loop (call state machine) │
   └──────────────────────────────────────────────┘  └─────────────────────────────────┘
```

Key structural choices:

- **The Agent entity is forge's** (`agents` table, H1.1) — ranch and
  mule reference agents by id; the Supabase `agents` mirror (H1.4)
  exists only so phone clients can render the registry offline of the
  daemon.
- **Cards are the approval currency.** Anything needing a human
  decision — policy ask, belief approval, research report, web sign-in
  — rides the SAME lane: forge `ranch_tool_request` → ranchd card lane
  → `AgentAskRequest` broadcast → any surface answers →
  `AgentAskAnswer` → daemon posts to `/ranch-tools/{id}/result` →
  forge applies the transition. The card's `tool` field is the kind
  (`policy_ask`, `memory_review`, `research_report`, `web_login`).
- **Proactivity is machine-local; Supabase only carries
  presentation.** Wakes fire mule→forge directly; the relay carries
  frames (notifications, ActivityPush, cards), never agent I/O.
- **Pause halts new work, not observability** (H5.3): paused agents
  reject new turns (409), drop scheduled fires, but keep reads open —
  pending asks stay visible everywhere.

## 2. Sequence: a woken turn (webhook → agent)

The always-on loop: an external event wakes an agent, the agent works,
the user is notified.

```
webhook source      mule            forge             ranchd           phone/TUI
     │  POST          │                │                  │                │
     │───────────────►│                │                  │                │
     │  /api/v1/wakes/fire             │                  │                │
     │  {kind:webhook, │               │                  │                │
     │   source,tag}   │ render prompt_template            │                │
     │                │ (missingkey=   │                  │                │
     │                │  error)        │                  │                │
     │                │ rate_limit_per_hour? ── drop+log──┤                │
     │                │                │                  │                │
     │                │  POST /agents/:id/conversations/:cid/messages      │
     │                │  ("[mule wake <id>] …")           │                │
     │                │───────────────►│                  │                │
     │                │                │ durable turn:    │                │
     │                │                │ claim+insert     │                │
     │                │                │ (one txn) →      │                │
     │                │                │ harness submit   │                │
     │                │                │                  │                │
     │                │  (job.completed via engine hook)  │                │
     │  wake_log row ─┼────────────────┼──────────────────►│ ActivityPush  │
     │                │                │                  │ (debounced)   │
     │                │                │  turn_end bus ───┼──────────────►│ notification:
     │                │                │                  │               │ agent "done"
     │                │                │  episode captured │               │ (auto, H4.2)
     │                │                │  (turn-end)       │               │
```

The same diagram works for cron (the 30 s cron-lane scan in mule
plays the role of the webhook ingress), for job wakes (a mule job
submits the message instead of fire-ing), and for memory-trigger
wakes (mule's forwarder lane polls forge's trigger queue every 30 s
and fires the wake IN-PROCESS, then ACKs it).

## 3. Sequence: an approval flow (policy ask)

Constraining agents from OUTSIDE the agent — the dots safety lesson
(research §2.2): the policy lives in mule, not in the agent's prompt.

```
agent turn (forge harness)      mule policy          forge API          ranchd           phone
     │ tool call: bash "rm …"    │                    │                   │               │
     │ before_tool hook          │                    │                   │               │
     │── GET/POST evaluate ─────►│                    │                   │               │
     │   {agent_id, tool, input} │ action_hash =      │                   │               │
     │                           │  sha256(tool+      │                   │               │
     │                           │  canonical input)  │                   │               │
     │                           │ memo hit? → return │                   │               │
     │                           │ memo miss →        │                   │               │
     │                           │  most-specific     │                   │               │
     │                           │  rule: DENY? → deny│                   │               │
     │◄──────────── verdict ─────│                    │                   │               │
     │   deny: tool blocked,     │                    │                   │               │
     │   reason visible to model │                    │                   │               │
     │   ask: POST              │                    │                   │               │
     │───────────────────────────┼───────────────────►│                   │               │
     │                           │                    │ /sessions/{id}/    │               │
     │                           │                    │ policy-ask →      │               │
     │                           │                    │ RanchToolQueue    │               │
     │                           │                    │  {kind: policy_ask,│               │
     │                           │                    │   rule_id, reason}│               │
     │                           │                    │── SSE ───────────►│ card lane     │
     │                           │                    │                   │ AgentAskRequest│
     │                           │                    │                   │ "Allow this    │
     │                           │                    │                   │  action?" ────►│
     │   (turn blocks in the     │                    │                   │ [Allow] [Deny] │
     │    harness, ≤ 60 s TTL)   │                    │                   │◄──────────────│
     │                           │                    │                   │ AgentAskAnswer │
     │◄──── POST /ranch-tools/{id}/result {allow} ────┼───────────────────┤ (2 s poll)     │
     │ on allow: memo write ────►│ upsertMemo         │                   │               │
     │ (same action never        │ (the evaluator's   │                   │               │
     │  asks again)              │  own path — no     │                   │               │
     │                           │  drift)            │                   │               │
```

Fail-closed: any evaluate error/timeout blocks the tool — a policy
deployment can never be bypassed by a flaky network. The TUI answers
from the pending-ask banner; the phone from the notification button
(PendingIntent → `AgentAskAnswer` — works backgrounded/locked).

## 4. Sequence: a reflection loop (learning)

Learning is a mule WORKFLOW — inspectable, approvable, versioned.
Belief changes surface as cards; nothing is ever silently written.

```
forge (durable)        mule reflection workflow        forge memory API       surfaces
     │ turn ends        │  (examples/reflection:        │                      │
     │── episode capture┤   pull → check → submit,      │                      │
     │  (H4.2, in forge)│   4 wasm modules + 1 agent    │                      │
     │  summary,        │   step for cheap inference)   │                      │
     │  provenance,     │                                │                      │
     │  redaction ─────►│ cron 0 3 * * * wake           │                      │
     │  memory.episodes │──────────►│                   │                      │
     │                  │ pull episodes (since last) ──►│ GET /memory/search   │
     │                  │ check: dup (string+cosine)    │                      │
     │                  │       contradiction (markers  │                      │
     │                  │       + token overlap)        │                      │
     │                  │       decay (conf·e^(-λ·age)) │                      │
     │                  │ submit: POST /agents/:id/     │                      │
     │                  │   memory/beliefs/proposals ──►│ near-dup gate        │
     │                  │                                │ → pending belief   │
     │                  │                                │ → memory_review    │
     │                  │                                │   card ────────────►│ "I've
     │                  │                                │                    │  learned:
     │                  │                                │                    │  … keep?"
     │                  │                                │◄── Keep/Forget ─────│ (phone
     │                  │                                │   (free text =     │  button;
     │                  │                                │    edit content)   │  TUI banner)
     │◄── belief active ┼────────────────────────────────┤                    │
     │ memory:beliefs prompt section now carries it:     │                    │
     │ top-15 by confidence + top-5 retrieved vs the     │                    │
     │ current message — in EVERY subsequent turn        │                    │
```

Watch-triggered wake (H4.5): a kept belief can carry `watch:
{match, wake_id}`; when a captured episode matches, forge queues a
`memory_trigger_queue` row, mule's forwarder lane polls it every 30 s
and fires the named wake (the §2 diagram) — then ACKs with
`POST …/triggers/:tid/consumed`. Cooldown (default 24 h) per belief.

## 5. Sequence: a voice call (H6.3/H6.4)

The call media path bypasses the daemon on purpose — no ranch frames,
no PTY; mule owns the state machine, the surfaces just pump audio.

```
phone / `ranch call`      mule voice loop (job)              forge
     │ create mule job                │                       │
     │ (workflow w/ voice_call step)  │                       │
     │══ in WS ══════════════════════►│ listening             │
     │ (16 kHz s16le ~30 ms frames,   │ energy VAD            │
     │  or {"text":…} typed fallback) │ utterance endpoint    │
     │                                │ (silence ≥500 ms AND  │
     │  TTS PCM (24 kHz) ◄════════════┤  ratio ≥3.0)          │
     │  {"assistant":…} frames ◄──────┤ thinking:            │
     │                                │  POST /messages       │
     │                                │  (mode: steer;        │
     │                                │   "[voice call] …") ─►│ agent turn
     │  BARGE-IN: mic speech while    │                       │
     │  TTS plays → 3-frame RMS       │                       │
     │  hysteresis trigger →          │                       │
     │  flushOut drains out-queue +   │                       │
     │  {"flush":true} ◄──────────────┤                       │
     │  (TTS stops <150 ms;           │                       │
     │   echo guard: out gain zeroed  │                       │
     │   while local mic hot)         │                       │
     │ {"hangup":true} / socket close►│ job → terminal        │
```

In-turn steer (v2): speech overlapping the *thinking* phase is
endpointed and submitted fire-and-forget into the in-flight turn —
the in-flight turn owns the speaker, so no TTS answer comes for it
(forge has no partial-prompt/cancel surface; v1 keeps the utterance
tagged and the delivery mode in the step config).

## 6. Where each surface sits

| surface | registry | cards | activity | voice |
|---|---|---|---|---|
| TUI (`client.rs`) | Agents section | pending-ask banner + card prompts | Activity section (Tab; `p`/`P` pause) | `ranch call <agent>` |
| web (`mobile/` Expo) | Agents tab | AgentAsk prompts | activity tab | hold-to-talk STT (forge Parakeet) |
| native Android (`mobile-native/`) | registry screen + ＋Agent | notifications with action buttons (works locked/backgrounded) | Settings → Tools → Activity; pause herd/agent | CallActivity (mic ⇄ mule WebSockets) + STT composer input |
| CLI | `ranch agent <name> [msg]`, `ranch agents new`, `ranch pause\|unpause [AGENT]` | — | — | `ranch call <agent>` |
| `tools/relay-test.mjs` | prints the new frames | | | — |
