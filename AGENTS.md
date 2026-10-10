# Ranch — Agent Instructions

A workbench for humans and agents, built on a terminal multiplexer.
Long-lived terminal sessions on your machine, reachable from a local
terminal (Linux-first), a browser, and an Android app. This is a tmux
**replacement**, not a companion; Forge agent sessions and Mule
workflows are first-class pane types.

## What THIS checkout is (ranch-2 ≠ ranch)

This directory is `~/src/ranch-2/`, a deep copy of `~/src/ranch/`.
It exists as an isolated environment for the **Herd** initiative
(always-on, always-learning agents — see next section). `~/src/ranch/`
is the live/production checkout running on this machine — **do not
touch it**; all new work happens here. Treat this repo as the future
center of gravity; divergence from `~/src/ranch/` is expected and
desired.

## The Herd initiative (why you're probably here)

Goal: an open, **self-hosted** answer to OpenAI's *Dots* — multiple
always-on agents per user that interact with each other and learn from
themselves and others. The full concept, research, gap analysis,
architecture, and build order live in:

- **`docs/research/always-on-agents-research.md`** — read this first;
  it is the source of truth for the concept (annotated sources:
  OpenAI Dots + safety post, Pi Durable RFC, audits of forge/mule/ranch).
- **`docs/PLAN-HERD.md`** — the Herd implementation plan (H0–H6
  phases with per-task file/line anchors, demo scripts, risk
  register, status ledger). Read it before doing Herd work; update
  its status ledger as phases land.

The one-paragraph version:

- **Division of labor:** *forge hosts the agents' lives, mule scripts
  their behavior, ranch is everywhere they can be seen and heard.*
- **The Agent entity** (codename: a "dot" in the herd) becomes a
  first-class row in forge — `agents(id, name, avatar, owner,
  home_machine, primary_profile, visibility, memory_scope)` — distinct
  from forge sessions (conversations) and ranch panes (surfaces). One
  agent, many conversations, many surfaces.
- **Forge refactor:** adopt pi-durable's semantic model (durable tasks
  + checkpoints, ownership trees, background tasks, hooks, documents,
  compaction, malleable extensions) on **Postgres** (pi-durable's
  storage interface was designed for this; conformance suite = porting
  spec). This *deletes* complexity: resume.rs, session_replay.rs, the
  harness/executor dual-writer split, idle-timeout process babysitting.
- **Learning is the differentiator:** episodic + semantic + org memory
  in Postgres/pgvector, written by **reflection loops that are mule
  workflows** (inspectable, approvable, versioned). Belief changes
  surface as approval cards via `ranch_ask`.
- **Mule becomes the behavior/policy plane:** wake policies (cron,
  event, webhook, memory-trigger), reflection/reporting workflows, and
  the custom-rules / auto-review policy engine that constrains agents
  from *outside* (via forge hooks), mirroring dots' safety separation.
- **Ranch becomes presence:** an agent registry (talk to an agent from
  any surface), agent-scoped mobile notifications + voice input
  (native Android client's foreground-service plumbing already exists),
  an Activity View for background/proactive work, and a voice loop over
  forge's Parakeet-STT/Kokoro-TTS proxies.

**Posture:** open source, self-hosted, own your own infra. Cost/scale
budgets are explicitly out of scope — if you see budget/pricing
reasoning in older drafts or upstream sources, ignore it; it was
removed from the research doc on purpose.

**Durable-runtime decision (settled 2026-10-06):** use
pi-durable upstream (`@earendil-works/pi-durable`) directly in a Node
harness process — no Rust reimplementation. Recorded in
`docs/PLAN-HERD.md` §H0.3, with the kill-criteria that would reopen
it.

---

Read before making non-trivial changes (they are the source of truth;
keep them in sync when behavior changes):

- `docs/research/always-on-agents-research.md` — the Herd initiative:
  target concept, tooling audits, gap analysis, concept architecture,
  forge refactor plan, build order.
- `docs/SPEC.md` — the target system: architecture, tenants, the forge/
  mule/webhook/agent-tool surfaces, security model.
- `docs/PLAN.md` — the implementation plan, grounded in the code.
- `docs/PROTOCOL.md` — wire protocol (shipped frames + §8 planned
  frame families).
- `docs/design/` — design docs with diagrams: agent-tools,
  webhook-receiver, workflows-and-triggers, agent-builder.
- `docs/history/` — archived spec versions and the milestone build log
  (M0–M11 completion notes; historical record, do not update).

New documentation goes in SPEC/PLAN/PROTOCOL/design; concept/initiative
research goes in `docs/research/`. Milestone completion notes continue
in the build log (`docs/history/`, append a dated entry) rather than
the target-system spec.

## Repository layout

- `crates/ranch-protocol/` — frame types, JSON-lines framing, chunking
  (no I/O; pure data + tests). The one schema shared by every transport.
  Planned frame families (agent tools, profiles, workflows, triggers,
  webhooks) are spec'd in PROTOCOL.md §8 before they ship.
- `crates/ranch-vt/` — hand-rolled FFI wrapper around `libghostty-vt`
  (pinned ghostty source). Server-side terminal emulation, one VT per
  pane. `build.rs` links against `vendor/lib/libghostty-vt.so`.
  The daemon side: PTY manager, one ghostty-vt per pane, 30 ms
  coalescing tick, session/window/pane registry, `state.json`, local
  unix-socket JSON-lines server, relay thread, Forge worker, local
  `pi --mode rpc` harness.
- `crates/ranch/` — the single binary: `main.rs` (dispatch: client vs
  daemon via argv0/subcommand), `client.rs` (CLI + attach TUI: ratatui +
  crossterm, key encoding via ghostty-vt), `daemon.rs`/`forge.rs`/
  `pilocal.rs`/`relay.rs` (ranchd). Plain `ranch` in a TTY opens the
  interactive session manager/dashboard.
- `mobile/` — Expo/React Native app, **Android only**. See
  `mobile/AGENTS.md` (and the versioned Expo v57 docs) before touching
  it. `mobile/CLAUDE.md` just points at `mobile/AGENTS.md`.
- `mobile-native/` — native Android (Kotlin) client, the replacement for
  `mobile/`. (1) A foreground service holds the Supabase Realtime socket so
  agent-event notifications fire in the background. (2) A session/terminal
  screen: session list → attach → PTY grid (SGR cell renderer + cursor,
  sentinel-EditText key capture + special-key row) or agent chat pane
  (`ChatSend`), plus `Resize`/`PaneSelect`/seq-gap resync. Speaks the same
  wire protocol / private channel as every other client (outbound frames as
  Realtime `broadcast`). See `mobile-native/README.md` + `BUILD-APK.md`.
  `applicationId` is `dev.ranch.android` (coexists with the RN app).
  Build: `cd mobile-native && ./gradlew assembleRelease` (AGP 8.12.0,
  Gradle 9.4.1, Kotlin 2.2.10, SDK 35, JDK 17).
- `supabase/migrations/` — schema + RLS (machines, sessions mirror,
  Realtime channel gating, self-serve `register_machine` RPC);
  `supabase/functions/` will hold the webhook-relay edge function
  (PLAN Phase E).
- `docs/` — SPEC (target system), PLAN, PROTOCOL, design/ diagrams,
  research/ (initiative concepts — currently the Herd/always-on-agents
  research doc), history/ build log.
- `systemd/ranchd.service` — user unit for the daemon.
- `tools/relay-test.mjs` — raw Supabase Realtime test client.
- `vendor/ghostty/` — shallow clone of pinned ghostty source (gitignored).
- `.spike/` — M0 validation spikes (`vt-spike`, `zt`).

## Build & run

- **Single binary.** `ranch` is the client AND the daemon — the daemon
  runs via `ranch daemon`, a `ranchd` argv0 symlink, or `--daemon`.
  It is statically linked (musl + the static `libghostty-vt.a` archive
  built from the pinned ghostty source — needs Zig 0.16). The shared
  lib path (`vendor/lib/libghostty-vt.so`) is the glibc fallback only.
- `make build` — static binary at
  `target/x86_64-unknown-linux-musl/release/ranch` (~21 MB, zero
  runtime deps).
- `make install` — copies to `~/.local/bin/ranch` + `ranchd` symlink.
- `make service` — installs + starts the `ranchd` systemd user unit.
- `make run` — run the daemon in the foreground.
- `make test` — `cargo test`. `make lint` — `cargo clippy --all-targets`.
- **`ranch upgrade`** — hot-upgrade the running daemon in place (re-exec
  with fd inheritance; sessions, panes, and agent children survive — see
  "Key invariants"). Run after `make install` for zero-downtime deploys.
- One-time: `make setup` (clone ghostty, install Zig, build the VT lib).

Rust workspace: edition 2024, stable toolchain (rustc 1.98 era).

## Key invariants & gotchas

- **Server-side emulation.** The daemon owns one `libghostty-vt` per pane;
  clients render the semantic grid and send encoded keys. Never move
  emulation to the client. `update` frames are "these rows are now
  exactly this" and are droppable; a `seq` gap ⇒ client sends `resync`.
- **Same frame schema on both transports.** Local unix socket
  (`~/.local/state/ranch/daemon.sock`, mode 0600, JSON-lines) and the
  Supabase Realtime relay use identical frames. Relay is enabled only when
  `~/.config/ranch/daemon.toml` exists (written by `ranch register`);
  otherwise the daemon is local-only and zero frames leave the machine.
- **Relay = "just another client".** `relay.rs` bridges WS ⇄ the poll
  loop through two pipe fds, and a forge worker gets its OWN pipe pair —
  sharing the relay's pair would silently route agent output to Supabase.
- **Realtime gotcha:** `channel.send()` before the channel is SUBSCRIBED is
  silently dropped. `Relay.join()` (both daemon and mobile) must await the
  status transition. Private channels are RLS-gated via `realtime.messages`
  (migration 0003).
- **Ghostty FFI quirks** (from M0/M1/M2.9): feed the decoder
  `&buf[..r]` not the full stack buffer; the formatter options struct has
  a nested `extra` (not `i32`); cursor `x` is a char index not a byte
  offset; the formatter emits the whole scrollable area so slice the last
  `rows` lines for the screen; pin the viewport to the bottom after
  writes. Don't regress these.
- **libghostty-vt** has runtime SONAME `libghostty-vt.so.0` — keep the
  symlink; deps are libc/libm only.
- **Windows/panes:** the daemon keeps a weighted binary split tree
  (`Layout::Split {dir, pct}`) per **window**; every pane's PTY is sized to
  its leaf (`TIOCSWINSZ` → SIGWINCH). Canonical size follows the
  most-recent-active client. Panes keep their PTY/VT state across swaps;
  only the rectangle trades. A session dies when its LAST pane closes —
  PTY *and* chat panes count (`s.panes.is_empty() && s.chats.is_empty()`);
  `s.panes` alone is always empty for agent sessions, so checking only it
  kills a 2-pane agent session on the first `Ctrl-B x`. Session death
  broadcasts `Meta kind="exited"` (TUI hops to the next session or
  exits). Killed pi children go on `orphans` for reaping
  (`LocalPi::kill` returns the pid) or they zombie for the daemon's life.
- **Local `pi` panes:** the daemon resolves the `pi` binary via
  `pi_bin`/`RANCH_PI_BIN`, then `~/.local/bin/pi` / mise shims, then PATH
  — needed because the systemd daemon's PATH is minimal.
- **Agent panes** (kind `forge` / `pi`): no PTY for chat panes. Forge
  conversation lives in forge's `messages` table, polled/streamed by the
  `forge.rs` worker and emitted as `chat` frames. Local `pi` panes spawn
  `pi --mode rpc` in the pane cwd and map RPC events to the same chat rows.
  **pi errors surface as `⚠ …` assistant chat rows** — pi never emits a
  `type:"error"` RPC event; failures arrive as `auto_retry_end
  {success:false, finalError}`, `message_end` with `stopReason:"error"`,
  or `extension_error`, and `pilocal.rs` maps all of them (plus retry
  attempts via `auto_retry_start`) to error rows so every client — TUI,
  web, and both mobile apps — shows them. Compact failures are ⚠ rows
  too (user-initiated AND the auto path). Auto compact+retry: when a
  turn dies with a context-exhaustion error (`stopReason:"error"` +
  overflow-ish `errorMessage`, incl. bare "400/413 status code
  (no body)" — pi's own overflow detection misses those), the reader
  issues `compact` and re-sends the last prompt once; a failed auto
  compact emits a "too large to compact" row instead of looping. Both
  mobile notification
  engines treat `⚠`-prefixed assistant rows as a default-on `errors`
  notification that suppresses that pane's `turn_end` notification (the
  error IS the turn-end news).
  A failed attach (e.g. forge down) must flash an error in the status bar,
  **not** `die()` the whole TUI (M8.4).
- **Planned workers follow the worker pattern** (PLAN.md): any new
  long-lived integration (mule, trigger scheduler, webhook handling) is
  a worker thread with its OWN pipe pair, jobs via `mpsc`, frames
  broadcast to clients like any other client frame, restart-safe across
  hot upgrade.
- **Feature parity is a requirement**: every user-facing capability
  ships on TUI, web, and mobile (or is explicitly daemon-side). PLAN.md
  tracks parity per phase.
- **Session persistence & hot upgrade (M10).** Two layers, both live:
  - **Tier 1 (cold restart):** `state.json` is a restore source, not just
    an observability dump. On boot the daemon rebuilds every recorded
    session: forge-chat panes re-subscribe their SSE watch (history
    replays), local-`pi` panes respawn `pi --mode rpc` in the recorded cwd
    and `switch_session` into the recorded conversation, shell panes get
    fresh shells in their last cwd. Session/pane ids are kept. Running
    programs + PTY scrollback are inherently lost.
    **pi session-file gotcha:** pi lazy-creates its session JSONL on the
    first message and answers RPCs in order, so the spawn-time
    `get_state` can reply *after* our `switch_session` with the fresh
    auto-created path. `LocalPi::switch_session` pins the target path
    and the reader ignores a differing `get_state` reply until pi has
    echoed the pin (`pin_unconfirmed`). `resolve_restore_file` drops
    recorded paths that don't exist on disk rather than switching pi
    into a phantom file.
  - **Tier 2 (hot upgrade, zero pane death):** `ranch upgrade` (CLI) or
    the `Upgrade` frame (phone: "upgrade" on the sessions screen) makes
    the running daemon execve ITSELF with `--inherit <manifest>` — PTY
    master fds, the listening socket, and `pi --mode rpc` child pipes are
    CLOEXEC-cleared and survive the exec. Children keep their pids; no
    pane, shell, or agent is interrupted. Threads (relay, forge worker,
    pi readers) die with the exec and restart reconnect-safe. On any
    inherit failure the daemon falls back to Tier-1 cold restore.
  - **Default to `ranch upgrade`** when a daemon restart is needed
    (binary change, config tweak that needs a reload). `systemctl --user
    restart ranchd` is the fallback — but remember a restart from INSIDE
    a ranch pane kills that pane's agent mid-command; agents working in
    a ranch pane must use the Upgrade frame (or ask the user) instead of
    restarting the daemon themselves.

## Conventions

- CLI: `ranch <new|agent|agents|pi|resume|ls|attach|kill|rename|split|switch|pause|unpause|call|upgrade|
  daemon|register|login|config|machines|cloud> [args]`; `ranchd` argv0 =
  daemon. Plain `ranch` in a TTY opens the dashboard. Inside attach,
  `Ctrl-B` is the tmux-style prefix. `pause`/`unpause [AGENT]` are the
  H5.3 herd kill switch (`AgentPause` frame; no agent = all known
  agents — "unpause" because `resume` is the session-resume command).
  `agents new` is the H6.5 specialist-dot provisioning wizard (forge
  agent row + optional mule cron wake; the mobile ＋Agent form speaks
  the `AgentProvision` frame — the daemon runs the same core,
  `agents_new.rs::provision`).
- tmux semantics are deliberate: `&`/`k` = window-kill, session-level kill
  is `:kill-session` / dashboard / phone long-press.
- Config: `~/.config/ranch/daemon.toml` (0600: relay machine key, forge
  `forge_url`/`forge_api_key`/`forge_profile_id`, mule endpoints). Client
  state: `~/.config/ranch/config.json` + `user.json` (0600). Env overrides:
  `RANCH_SUPABASE_URL`, `RANCH_ANON_KEY`, `RANCH_SOCKET`.
- Security: local socket 0600 same-uid only; machine key shown once at
  `ranch register`, only its SHA-256 is stored; terminal content transits
  Supabase unencrypted E2E in MVP (documented, accepted risk).
