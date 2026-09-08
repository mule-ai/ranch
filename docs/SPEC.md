# Ranch — Spec

Status: draft v0.1 (research + architecture + MVP scope). No code yet.

## 1. What Ranch is

Ranch is a personal terminal multiplexer with a cloud relay. It owns
**long-lived terminal sessions** on your machine; each session contains
one or more **panes** (PTY-backed terminal instances). You attach to a
pane from:

- a **local terminal** on the same machine (Linux first),
- a **mobile app** over the cloud relay,
- (later) web or any other client.

When you disconnect, panes keep running. When you reconnect — anywhere,
any device — the terminal state (screen, scrollback, cursor, running
process) is exactly where you left it. Simple tmux-style multiplexing is
the core product; **Forge** agent sessions and **Mule** workflows are
first-class pane types, not afterthoughts.

The pitch: *your ranch, your machines, your agents.*

### Inspiration

[Superlogical](https://www.superlogical.com/) (Mitchell Hashimoto et
al.) is building "the multiplexer for all work": a terminal multiplexer
with durable sessions that span devices, accessible via web and native
macOS/iOS clients, with sharing built in from the start. Ranch takes the
same core idea — a durable session as the unit of work — and builds the
personal/developer-tool version of it on top of the pieces we already
own (Ghostty's terminal engine, Forge, Mule, Supabase).

## 2. Research notes

### 2.1 libghostty

Ghostty ships its terminal emulation as an embeddable C library,
**`libghostty-vt`** (from `include/ghostty/vt.h`): parsing of terminal
escape sequences, terminal state (screen grid, scrollback, cursor,
styles), reflow on resize, key/mouse encoding (Kitty keyboard protocol,
SGR mouse), OSC/SGR parsing, formatting (text/VT/HTML), search, and
snapshot/restore. Functional behavior is "extremely stable" (it is what
the Ghostty GUI uses); **API signatures are still in flux** — pin to a
ghostty commit.

Available today for C and Zig on macOS, Linux, Windows, WASM. Ecosystem:

| crate | what it is |
|---|---|
| `libghostty-vt-sys` 0.2.1 | raw FFI bindings to the C API |
| `libghostty-vt` 0.2.1 | safe Rust wrapper |
| `gpui-libghostty` 0.2.1 | GPUI terminal component (full libghostty, wgpu renderer) |
| `ratatui-ghostty` 0.2.0 | ratatui widget driven by ghostty-vt |
| `poltergeist` 0.4.0 | session manager for Ghostty panes — closest prior art to this product shape |

Reference project: **Ghostling** (ghostty-org) — minimal complete app
embedding libghostty. Smaller examples live in ghostty's `example/` dir.

**Decision:** the ranch daemon embeds `libghostty-vt` and does terminal
emulation *server-side* (one VT instance per pane). Clients render
semantic screen state, not raw bytes. See §4 for why.

### 2.2 Prior art

- **tmux**: byte-stream multiplexing; each client emulates locally from
  the byte stream; server keeps a ring buffer for "attach replay."
  Works, but clients must carry a full VT implementation and replay is
  lossy for heavy output; mobile clients are the weak spot.
- **Zellij**: Rust, plugin TUI, similar client-emulation model.
- **poltergeist / Ghostling**: prove the libghostty embedding path.
- **Superlogical's stated direction** (durable session, native clients,
  built-in sharing) is the product thesis we're following.

### 2.3 Forge (../forge)

Rust/axum API server, default `http://localhost:8080`, auth via
`X-API-Key`, PostgreSQL, deployed as systemd service (`forge-api`).
Relevant surface:

- `GET/POST /sessions`, `GET/PATCH/DELETE /sessions/{id}` — sessions
  carry `user_id`, name, `working_dir` (`/forge/sessions/<uuid>`).
- `GET /messages?session_id=…`, `POST /messages` — message stream.
- `GET /sessions/{id}/events?since=<seq>` — **SSE live stream** of new
  messages + `agent_end`.
- `POST /v1/chat/completions` — OpenAI-compatible surface, stateful mode
  `forge:<session-id>`.
- Durable resume: killing a session's `pi` subprocess is recoverable;
  reactivation replays the working tree and loads the session jsonl.

Forge sessions are long-lived, addressable, and have a clean live-event
stream — ideal for embedding as ranch panes.

### 2.4 Mule (../mule)

Go API + PostgreSQL, docker-compose deployment, default `:8080`.
Relevant surface:

- `/api/v1/workflows` (CRUD) + `/api/v1/workflows/{id}/steps` (CRUD).
- `/api/v1/agents`, `/tools`, `/skills`, `/providers`, `/settings`.
- `/v1/chat/completions` — OpenAI-compatible.
- `WS /ws` — WebSocket hub; agent executions stream over it
  (`message_update`, `tool_execution`, extension-UI events).
- Agent runtime is pi-in-RPC-mode (replaced ADK), so Mule workflows
  already speak the same agent language as Forge.

### 2.5 Supabase

Used for: **Auth** (JWT, email/OAuth), **Postgres** (registry:
machines, sessions mirror), **Realtime** (WebSocket broadcast channels
with RLS-protected private channels) as the relay. No edge functions in
MVP — the relay is pure pass-through; all terminal logic lives in the
daemon and clients.

Known constraints to verify at M2: Realtime broadcast message size
limit (~28 KB payload), concurrent-connection allowances on the chosen
tier, and end-to-end latency. Mitigations: protocol chunking (§6),
seq-gap re-sync, and coalesced (droppable) frames.

## 3. Goals & non-goals (MVP)

### Goals

1. **G1 — Daemon**: `ranchd` owns N persistent sessions, each with 1..N
   panes; panes survive all clients disconnecting; scrollback retained.
2. **G2 — Simple multiplexing**: create/list/rename/kill sessions;
   split panes (h/v); switch panes; tmux-light key handling.
3. **G3 — Local terminal**: `ranch` CLI + attach UI on Linux, connecting
   to the local daemon; ghostty-grade rendering of server-emulated
   grids; native scrollback, selection, resize.
4. **G4 — Cloud relay**: daemon registers a machine with Supabase;
   mobile app authenticates and attaches to any of the owner's
   sessions over Supabase Realtime; full snapshot + live incremental
   updates + input in both directions.
5. **G5 — Mobile app**: iOS-first (Expo/React Native) — list
   machines/sessions, attach to a pane, scrollback, basic keys.
6. **G6 — Forge first-class**: browse Forge sessions in ranch lists
   (name + live status from the API); `ranch attach forge:<id>` opens a
   pane that is that agent session (live TUI); status badges update
   live.
7. **G7 — Mule first-class**: list Mule workflows; run a workflow into a
   new ranch pane that streams its live output; visible in mobile lists.

### Non-goals (MVP)

- Web client (protocol supports it; ship later).
- Multi-user sharing / per-pane ACLs beyond owner-only (Superlogical's
  "sharing built in" is explicitly out; design must not preclude it).
- End-to-end encryption of terminal content (it transits Supabase;
  acceptable for a personal tool — document it; E2E later).
- P2P direct transport that bypasses the relay (relay-only in MVP).
- macOS/Windows local terminal clients (Linux first; architecture must
  not preclude other OSes).
- GPU-accelerated renderer for the local client (good text renderer is
  enough for MVP).
- tmux compatibility (config, control mode) — "tmux-style," not
  "tmux-compatible."
- Session recording / playback (scrollback ≠ recording).

## 4. Architecture

```
                 ┌──────────────────────────────┐
                 │  Supabase (cloud relay)      │
                 │  • Auth (JWT)                │
                 │  • Postgres: machines,       │
                 │    sessions (mirror)         │
                 │  • Realtime: private WS      │
                 │    channel per machine       │
                 └───────▲──────────────┬───────┘
              relay WS   │              │  attach WS (owner JWT)
                         │              │
┌────────────────────────┴──────────────┴────────────────────────┐
│  ranchd  (on your machine)                                      │
│                                                                 │
│  ┌────────────┐   ┌──────────────────────────────────────────┐  │
│  │ registry   │   │  session 1                               │  │
│  │ (sessions, │   │  ┌─────────┐ ┌─────────┐ ┌────────────┐  │  │
│  │  panes,    │   │  │ pane a  │ │ pane b  │ │ pane c     │  │  │
│  │  clients)  │   │  │ PTY ────┤ │ PTY ────┤ │ agent:forge│  │  │
│  └────────────┘   │  │ ghostty │ │ ghostty │ │ ghostty-vt │  │  │
│                    │  │ -vt VT  │ │ -vt VT  │ │ + pi TUI   │  │  │
│                    │  └─────────┘ └─────────┘ └────────────┘  │  │
│  ┌────────────┐   └──────────────────────────────────────────┘  │
│  │ local      │                                                 │
│  │ unix-sock  │   integrators: forge-api (:8080), mule (:8080)  │
│  │ server     │                                                 │
│  └─────▲──────┘                                                 │
└─────────┼───────────────────────────────────────────────────────┘
          │ unix socket (owner)
   ┌──────┴───────┐
   │ ranch (CLI + │        ┌──────────────┐
   │ attach UI)   │        │ mobile app   │
   └──────────────┘        │ (Expo, iOS)  │
                           └──────────────┘
```

### 4.1 Why server-side emulation

Each pane has exactly one source of truth for terminal state: a
`libghostty-vt` instance in the daemon fed by that pane's PTY. Clients
never emulate; they **render the current grid** and send **keys**.

Consequences:

- **Reconnect is exact.** Attach = fetch snapshot (grid + scrollback
  tail) + stream of incremental updates. No lossy byte-replay.
- **Clients are thin and uniform.** The mobile app, the local UI, and a
  future web client all do the same thing: paint cells, send keys.
- **Frames are droppable.** A screen update carries "these rows are
  now this." Drop a frame, catch up on the next; seq gaps trigger a
  re-sync. This is what makes a lossy cloud relay tolerable.
- **Refold is handled by Ghostty.** Resize resizes the canonical VT;
  every client just repaints.
- **Cost:** PTY output is parsed on the machine that owns it (cheap),
  and the wire carries semantic cells instead of raw bytes (usually
  smaller after coalescing, especially for TUIs that rewrite the whole
  screen).

Plan B (if `libghostty-vt`'s in-flux API proves too painful for MVP):
tmux-style byte-stream multiplexing with per-pane ring buffers and
client-side emulation; the daemon still owns the PTY. This keeps the
protocol shape (attach → replay → stream) but degrades mobile attach
quality. The spec's protocol (§6) is written so the daemon can later
switch back to grid frames with only payload changes.

### 4.2 The daemon (`ranchd`)

Rust, one process per machine, systemd unit. Responsibilities:

- **PTY manager**: spawn/kill processes in panes (`portable-pty` /
  `nix`). Pane command defaults to the user's shell; agent panes spawn
  the appropriate agent process (§7, §8).
- **VT owner**: one `libghostty-vt` terminal per pane; PTY output →
  `vt.write`; on each coalescing tick (~30 ms) extract dirty rows and
  cursor → `update` frames to all attached clients.
- **Session/pane registry**: in-memory + a small JSON state file
  (`~/.local/state/ranch/state.json`) so names/ids/pane layout survive
  `ranchd` restarts. (Live *processes* do not survive a `ranchd`
  restart in MVP — sessions are reaped; document this.)
- **Local server**: unix socket `~/.local/state/ranch/daemon.sock`
  (mode 0600); JSON-lines framing, same frame schema as the relay path.
- **Relay client**: persistent outbound WebSocket to Supabase Realtime
  private channel `realtime:machines:<machine_id>`; forwards
frames both ways with
  a small routing header (frame is addressed to/from a session+pane or
  to the control plane). Reconnect + resync logic lives here.
- **Integrators**: `forge` and `mule` clients (HTTP + SSE/WS) used for
  list/metadata/status and for spawning agent panes.

Config: `~/.config/ranch/daemon.toml` (machine name, relay url +
machine key, forge/mule endpoints, coalescing tick, scrollback lines).

### 4.3 Cloud relay (Supabase)

No custom cloud code in MVP.

- **Auth**: mobile app signs in with Supabase Auth (email + OAuth). The
  daemon does *not* use user auth; it registers with a **machine key**.
- **Schema** (see §9): `machines`, `sessions` (mirror).
- **Realtime**: one **private** broadcast channel per machine,
  `realtime:machines:<machine_id>`. Realtime validates the joining
  user's token against RLS on `realtime.messages` (migration 0003): only the machine key holder (daemon) and
  the owner's user JWT can join. The channel is a dumb pipe: all
  framing/semantics is Ranch's protocol.
- The `sessions` mirror lets the mobile app list sessions even when a
  machine is offline (stale, marked `offline` via last-seen heartbeat).

### 4.4 Local terminal (`ranch`, Linux first)

Rust binary, two jobs:

1. **CLI** (no TUI needed for MVP): `ranch sessions`, `ranch attach
   <ref>`, `ranch new [name]`, `ranch split <session> h|v`, `ranch kill
   <session>`, `ranch machines`, `ranch register` (prints machine key
   on first run), `ranch forge` (list forge sessions), `ranch mule`
   (list workflows). Talks to the daemon over the unix socket.
2. **Attach UI**: full-screen TUI that renders the pane's grid and
   forwards keyboard input. MVP renderer: **ratatui** painting the
   server grid (colors via SGR attributes, cursor, selection with
   scrollback). `libghostty-vt` is used for **key/mouse encoding**
   (Kitty protocol / SGR mouse) so input is encoded the Ghostty way.
   Multi-pane sessions render as a split layout (pane geometry is part
   of the snapshot).
   Later (post-MVP): a native winit+wgpu window, and optionally
   `gpui-libghostty` for a pixel-identical Ghostty surface.

Session reference syntax: `attach <name-or-id>` for shell sessions,
`attach forge:<forge-session-id>` for agent sessions,
`attach mule:<workflow-id>` for workflow runs.

### 4.5 Mobile app

Expo / React Native, **iOS first** (Android follows the same code).

- Sign in via Supabase Auth → see machines (online/offline) → sessions
  (shell + forge + mule, with status badges) → attach.
- Attach view: monospace grid of `Text` runs (rows of styled cell
  runs), pan/scroll for scrollback, hardware-keyboard input, minimal
  on-screen key row (esc, ctrl, arrows) for MVP.
- Forge panes: live agent TUI in MVP; structured chat rendering
  (from Forge SSE) is the natural post-MVP upgrade and should be
  designed for (frame types are extensible — see §6.6).
- Reconnect behavior: on resume, attach → snapshot → live.

## 5. Core concepts

- **machine** — one `ranchd` installation; identified by a machine key.
- **session** — a named group of panes + pane geometry; the durable
  unit. Kinds: `shell` (plain), `forge:<id>` (agent-bound),
  `mule:<workflow-id>` (workflow-bound).
- **pane** — one terminal instance: PTY (or, for forge/mule, a
  spawned agent process) + its ghostty-vt state + scrollback.
- **client** — anything attached (local UI, mobile, future web). A
  pane may have multiple simultaneous clients; all see identical state.
- **pane geometry** — for MVP: sessions hold a split layout
  (binary split tree); pane sizes derive from the session's canonical
  size, which is the size of the most-recently-active client (tmux
  rule: resize follows the biggest/most-recent client).

## 6. Wire protocol

Full details in `docs/PROTOCOL.md`. Summary:

- **Transport**: JSON-lines over (a) unix socket locally, (b) Supabase
  Realtime broadcast as `{"topic": "ranch:<machine>", "payload": <frame>}`.
- **Addressing**: every frame carries `to` = `{machine, session?,
  pane?}`; daemon and clients filter by address.
- **Frame kinds**: `hello` / `auth-ok`, `snapshot` (grid + scrollback
  tail + geometry + cursor), `update` (dirty rows + cursor, coalesced
  per tick), `input` (encoded key/mouse bytes or Kitty protocol seq),
  `resize`, `scrollback` (paged history), `sessions` (control:
  create/rename/kill/split), `meta` (titles, status), `resync`
  (request full re-snapshot after seq gap), `hb` (heartbeat).
- **Sequencing**: daemon keeps a monotonic `seq` per session; `update`
  frames carry it; gap ⇒ client sends `resync`.
- **Sizing**: frames are chunked to stay under the Realtime limit
  (~28 KB); `snapshot` is always chunked; `update` frames that would
  exceed the limit are split and marked as one atomic batch.

## 7. Forge integration (first-class)

Config in `daemon.toml`:

```toml
[forge]
url = "http://127.0.0.1:8080"
api_key = "sk_forge_..."        # or path to key file
```

Behavior:

- **Listing**: `GET /sessions` (owner-scoped) feeds ranch's session
  lists (local CLI and mobile). Each forge session shows `name`,
  `state` (active/idle, derived from last message age +
  `agent_end` on the SSE stream), and `working_dir`.
- **Attach**: `ranch attach forge:<id>` creates (or reuses) a ranch
  session bound to that forge session. Its pane spawns the agent
  process in the forge session's `working_dir` — MVP: `pi --session
  <jsonl>` (the same mechanism forge's own resume uses), which gives a
  live, interactive TUI of the agent. The pane is a normal PTY pane;
  ranch just knows its provenance.
- **Status**: daemon subscribes to `GET /sessions/{id}/events` (SSE)
  for attached forge panes and publishes `meta` frames
  (running/done/error + last message preview) so mobile lists show
  live agent state without attaching.
- **Post-MVP**: a structured "agent pane view" — render forge messages
  as chat (mobile-first) instead of the raw TUI; the SSE stream
  already carries everything needed.

## 8. Mule integration (first-class)

Config:

```toml
[mule]
url = "http://127.0.0.1:8080"    # or remote
# api key / auth header as needed
```

Behavior:

- **Listing**: `GET /api/v1/workflows` feeds ranch lists (name, step
  count, updated-at).
- **Run into a pane**: `ranch mule run <workflow-id> [--session <name>]`
  POSTs a workflow run and opens the Mule `WS /ws` stream for that
  run; the daemon tees the streamed events (agent `message_update` /
  `tool_execution` output, formatted as plain text) into a dedicated
  pane's ghostty-vt. The user watches the workflow live, locally or on
  their phone, and can scroll its full output as scrollback.
- **MVP fallback**: if the WS event format is too fluid to tee
  cleanly, the pane instead runs a small `ranch-mule-tail` helper that
  polls the run's outputs — same UX, simpler plumbing. (Decide at M3
  based on Mule's actual event schema.)

## 9. Data model (Supabase Postgres)

```sql
-- auth.users provided by Supabase
create table machines (
  id          uuid primary key default gen_random_uuid(),
  user_id     uuid not null references auth.users on delete cascade,
  key_hash    text not null unique,      -- sha256(machine key); key shown once
  name        text not null,
  created_at  timestamptz not null default now(),
  last_seen_at timestamptz
);

create table sessions (          -- registry mirror; truth lives in ranchd
  id           uuid primary key,   -- daemon-assigned session id
  machine_id   uuid not null references machines on delete cascade,
  name         text not null,
  kind         text not null default 'shell',  -- shell | forge | mule
  ref_id       text,                -- forge session id / mule workflow id
  created_at   timestamptz not null default now(),
  last_active_at timestamptz
);
```

RLS: owner-only read/write on both tables; `machines.key_hash` never
readable. The daemon authenticates to Realtime with the machine key
(Supabase custom JWT signed by a service-role key at `register` time,
or a long-lived token minted per machine — decide at M2; keep the
`machines` row the source of truth either way). Heartbeat: daemon
updates `last_seen_at` via a lightweight upsert every 30 s.

## 10. Security

- Local: unix socket 0600 + same-uid only. No network exposure.
- Machine key: shown once at `ranch register` (and in `daemon.toml`,
  0600); only its SHA-256 lives in Postgres; a leaked key is revoked by
  re-registering.
- Relay: private Realtime channel, RLS-gated to machine-owner.
- Terminal content transits Supabase **unencrypted end-to-end** in MVP
  (TLS in transit only). Documented, accepted risk for a personal
  tool; E2E (machine Ed25519 pubkey in registry, frames encrypted) is a
  designed-for-later item.
- Agent panes: the daemon's forge/mule API keys grant agent access;
  they are stored only in `daemon.toml` (0600) and never sent through
  the relay.

## 11. Repository layout & build

```
ranch/
├── Cargo.toml                  # workspace
├── flake.nix                   # dev shell + builds; pins ghostty commit
├── justfile
├── docs/
│   ├── SPEC.md                 # this file
│   └── PROTOCOL.md             # wire protocol reference
├── crates/
│   ├── ranch-protocol/         # frame types, framing, chunking (no I/O)
│   ├── ranch-daemon/           # ranchd: PTY, ghostty-vt, servers, integrators
│   ├── ranch-cli/              # ranch: CLI + attach TUI (ratatui)
│   └── ranch-mule-tail/        # (M3, if needed) workflow output tailer
├── mobile/                     # Expo app (M3)
├── systemd/ranchd.service
└── supabase/
    ├── migrations/             # schema + RLS
    └── config.toml
```

### 11.1 libghostty-vt build story (validated in M0)

Validated on Omarchy Linux x86_64 (rustc 1.98) on 2026-09-07:

- **Pinned commit**: `82232ecde55405559dec29c5466cb9e39938cb41` (shallow
  clone in `vendor/ghostty`, gitignored; record the pin in
  `justfile`/flake so it is reproducible).
- **Toolchain**: Zig 0.16.0 (prebuilt tarball, `zig-x86_64-linux-0.16.0.tar.xz`).
- **Build**: the vt shared lib builds as a side-effect of any example
  build: `cd vendor/ghostty/example/c-vt-stream && zig build` →
  `libghostty-vt.so` lands in that example's `.zig-cache/o/<hash>/`.
  For the product, a dedicated `justfile` target (or flake) will copy it
  to a stable path; the Zig build in `.spike/zt/` shows the minimal
  wrapper (path dependency on the pinned source).
- **Linking**: `libghostty-vt.so` has a runtime SONAME of
  `libghostty-vt.so.0` — ship a symlink (or `-Wl,-soname`/install_name) or
  the loader fails. Its only runtime deps are libc/libm.
- **Rust binding**: hand-rolled `extern "C"` declarations against
  `include/ghostty/vt/*.h` + `build.rs` that emits
  `cargo:rustc-link-search=native=` / `cargo:rustc-link-lib=dylib=ghostty-vt` /
  rpath. M0 proved this works; the published `libghostty-vt-sys` crates are
  an alternative but track their own upstream pin, so hand-rolled + pinned
  source is more controllable for MVP.

**M0 findings carried into M1** (from `.spike/vt-spike`):

1. **Formatter PLAIN is the MVP text source.**
   `ghostty_formatter_terminal_new` + `GHOSTTY_FORMATTER_FORMAT_PLAIN` +
   `ghostty_formatter_format_alloc` gives the full screen as plain text every
   tick; diffing it is cheap and correct. No per-cell traversal needed for
   the MVP renderer. Colored output later: `GHOSTTY_FORMATTER_FORMAT_VT`
   (emits escape sequences — can be forwarded to the local TUI verbatim and
   parsed once for the mobile client) or cell-level grid traversal
   (`ghostty_terminal_grid_ref` + `ghostty_grid_ref_row`). Decision deferred
   to M1: **MVP ships PLAIN-diff; wire the update frame as "rows of
   strings" not "cells",** so M2 can swap payload to styled cells without
   changing framing.
2. **Remaining FFI surface to wire in M1** (all present in pinned
   headers, none yet exercised):
   - `ghostty_terminal_resize(cols, rows, cw, ch)` → reflow (test with vim)
   - scrollback: `ghostty_terminal_grid_ref` with
     `GHOSTTY_POINT_TAG_HISTORY` points (plus `ghostty_grid_ref_row` / `..._cell`
     to read lines) + paged fetch; also `ghostty_terminal_scroll_viewport`
   - key encoding: `include/ghostty/vt/key.h` (Kitty keyboard protocol)
     for client input → bytes; mouse: `mouse.h` (SGR)
   - `ghostty_terminal_vt_write_until_ground` — useful if the daemon ever
     needs to inject its own sequences (e.g. pane titles)
   - `ghostty_terminal_snapshot*` — later, for cross-device
     state transfer instead of replaying scrollback
3. **PTY lifecycle** (openpty/fork/execvp/poll/waitpid-WNOHANG) is stable
   and dependency-free; wrap it in a small `pty` module in `ranch-daemon`
   with the same shapes the spike uses (setsid + tcsetpgrp + dup2 slave).
4. **Pane size policy** (spec §5): MVP uses "most-recent-active client wins"
   — a client's `resize` sets the canonical VT size; all attached clients
   re-render. Per-client virtual sizes are a post-MVP item.

Rust edition 2024, stable toolchain (matches forge: rustc 1.98).

## 12. MVP milestones

- **M0 — spike**: ✅ **DONE 2026-09-07 (PASS)** — `.spike/vt-spike`: Rust
  binary (zero cargo deps) drives a PTY shell, feeds output into
  `libghostty-vt`, diffs the formatted screen every 250 ms, emits
  changed rows. Pinned ghostty commit:
  `82232ecde55405559dec29c5466cb9e39938cb41` (libghostty-vt.so.0,
  deps: libc/libm only). Go/Plan-B gate: **go**.
- **M1 — local multiplexer (core)**: ✅ **DONE 2026-09-07** —
  `ranchd` + `ranch` CLI/attach, unix socket only. All sub-tasks
  delivered: `ranch-protocol` (frame types, chunking, 6 tests incl.
  multi-frame-in-one-read), `ranch-daemon` (pty FFI, ghostty-vt per
  pane, 30 ms tick, dirty-row diff, session/pane registry, state.json,
  unix-socket JSON-lines server, resize, scrollback ring buffer),
  `ranch-cli` (attach TUI via ratatui, key encoding, new/ls/attach/
  kill/rename/split/switch), `ranch-vt` FFI wrapper, systemd unit.
  **Acceptance PASS (7/7):** HelloOk, SessionsAck, attach snapshot,
  input round-trip (echo visible), state survives disconnect/reconnect
  (exact screen), resize triggers reflow with new dims, kill.
  Bugs found & fixed during M1: (a) daemon fed the full 64 KB stack
  buffer into the line decoder instead of `&buf[..r]` — stale NULs
  silently dropped coalesced frames; (b) ghostty formatter options
  struct had a wrong `extra` layout (`i32` vs the real nested struct)
  causing `rc=-2`; (c) cursor x is a char index, not a byte offset —
  TUI slicing panicked on multi-byte prompt chars.
- **M2 — relay + registration**: ✅ **DONE 2026-09-08** — Supabase
  project `ranch` (us-east-1), schema + RLS (migrations 0001–0003),
  `ranch register` (machine auth user + machines row + 0600
  daemon.toml), relay thread in ranchd (tungstenite WS + ureq REST:
  join/heartbeat/session-mirror/reconnect with backoff), raw test tool
  `tools/relay-test.mjs`. Key discovery: private Realtime channels are
  RLS-gated via probe inserts on `realtime.messages`, not the topic's
  table — migration 0003 adds read/insert policies keyed on
  `topic = 'machines:' || machine_id`. Relay client = a Client in the
  daemon's map with pipe fds (remote frames arrive like local ones).
  **Acceptance PASS:** frames flow machine→cloud→remote (HelloOk,
  Snapshot, Update with `echo hi` visible at the remote end), remote
  Input round-trip over the relay, remote SessionsKill (local + cloud
  mirror converge), session mirror + heartbeat visible via owner JWT
  while the daemon is offline (stale list), register/unregister.
  Deferred to M3: seq-gap auto-resync on the client side (protocol
  support exists; the test tool detects gaps and re-attaches).

- **M2.5 — multi-user OAuth + remote client (2026-09-08)**: identity
  model upgraded from "admin-provisioned" to self-serve. `ranch
  config` (one-time: project URL + anon key), `ranch login` (Google
  OAuth via browser PKCE + localhost callback; `ranch login <email>`
  as password fallback for headless machines), `ranch machines`
  (list devices, online dot via last_seen), `ranch cloud` (sessions
  across all machines from the mirror). `ranch register` no longer
  needs the service role — it calls the `register_machine`
  security-definer RPC (migration 0004) which mints the machine key,
  creates the machine auth user, and inserts the row; 0005 makes
  unregister also clean up the auth user. Multiple daemons per
  account fully supported (RLS: machines.user_id = auth.uid()).
  Client state: ~/.config/ranch/config.json (public) + user.json
  (0600, refreshable). Cloud config resolution: RANCH_SUPABASE_URL /
  RANCH_ANON_KEY env > config.json > baked-in defaults (the Ranch
  project's URL + anon key, which are public identifiers), so `ranch
  login` works out of the box for this project and self-hosters can
  point their builds elsewhere via `make` env or `ranch config`. Google provider must be enabled in the
  Supabase dashboard (external_google_enabled) + redirect
  http://localhost:8737/callback added to the allow-list.
- **M3 — mobile app**: Expo app, auth, machine/session lists, attach
  view with input, scrollback, status. Acceptance: attach to a live
  session on a remote machine from a phone; input works; reconnect on
  app resume is exact.
- **M4 — forge + mule first-class**: forge listing/status/attach
  (§7); mule listing + run-into-pane (§8). Acceptance: start a forge
  session locally, watch it from the phone; run a mule workflow into a
  pane and watch it live.

## 13. Risks & open questions

1. **libghostty-vt API churn.** M0 gate passed 2026-09-07 with pinned
   commit `82232ecb`; build + linking story validated (§11.1). Residual
   risk: API still in flux upstream — mitigated by pinning the source and
   hand-rolling FFI against the pinned headers. Plan B (byte-stream +
   client emulation) remains documented in §4.1 if the pin ever breaks.
2. **Supabase Realtime limits** (message size, throughput, tier
   pricing, latency under mobile network flakiness). Mitigation:
   chunking, coalescing, droppable updates + resync; measure at M2 and
   keep the transport interface swappable (a future P2P or
   self-hosted relay is a config change, not a rewrite).
3. **Machine-key auth to Realtime** — Supabase's story for
   non-user devices needs checking at M2 (service-role-minted JWT vs
   anon + RLS with a secret claim). Low-risk, one-weekend decision.
4. **Agent pane spawn semantics for forge** — using `pi --session`
   directly duplicates part of forge's harness (env, tool extension
   wiring). Alternative: add a tiny `forge attach <id>` interactive
   command to forge itself (small forge change; arguably the right
   home for it). Decide during M4.
5. **Mule WS event schema stability** for teeing into panes — see §8
   fallback.
6. **Canonical pane size with mixed clients** (phone 40-col, laptop
   200-col): follow most-recent-active-client rule; revisit if the
   phone-first workflow needs per-client virtual sizes.
