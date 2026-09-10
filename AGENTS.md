# Ranch — Agent Instructions

Personal terminal multiplexer with a cloud relay. Long-lived terminal
sessions on your machine, reachable from a local terminal (Linux-first),
an Android app, and — by design — from anywhere later. This is a tmux
**replacement**, not a companion; Forge agent sessions and Mule workflows
are first-class pane types.

Read `docs/SPEC.md` (architecture, security model, milestones M0–M4 and
M5–M9 completion notes) and `docs/PROTOCOL.md` (wire protocol) before
making non-trivial changes. They are the source of truth; keep them in
sync when behavior changes.

## Repository layout

- `crates/ranch-protocol/` — frame types, JSON-lines framing, chunking
  (no I/O; pure data + tests). The one schema shared by every transport.
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
- `supabase/migrations/` — schema + RLS (machines, sessions mirror,
  Realtime channel gating, self-serve `register_machine` RPC).
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
  only the rectangle trades.
- **Agent panes** (kind `forge` / `pi`): no PTY for chat panes. Forge
  conversation lives in forge's `messages` table, polled/streamed by the
  `forge.rs` worker and emitted as `chat` frames. Local `pi` panes spawn
  `pi --mode rpc` in the pane cwd and map RPC events to the same chat rows.
  A failed attach (e.g. forge down) must flash an error in the status bar,
  **not** `die()` the whole TUI (M8.4).
- **Session persistence & hot upgrade (M10).** Two layers, both live:
  - **Tier 1 (cold restart):** `state.json` is a restore source, not just
    an observability dump. On boot the daemon rebuilds every recorded
    session: forge-chat panes re-subscribe their SSE watch (history
    replays), local-`pi` panes respawn `pi --mode rpc` in the recorded cwd
    and `switch_session` into the recorded conversation, shell panes get
    fresh shells in their last cwd. Session/pane ids are kept. Running
    programs + PTY scrollback are inherently lost.
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

- CLI: `ranch <new|agent|pi|resume|ls|attach|kill|rename|split|switch|upgrade|
  daemon|register|login|config|machines|cloud> [args]`; `ranchd` argv0 =
  daemon. Plain `ranch` in a TTY opens the dashboard. Inside attach,
  `Ctrl-B` is the tmux-style prefix.
- tmux semantics are deliberate: `&`/`k` = window-kill, session-level kill
  is `:kill-session` / dashboard / phone long-press.
- Config: `~/.config/ranch/daemon.toml` (0600: relay machine key, forge
  `forge_url`/`forge_api_key`/`forge_profile_id`, mule endpoints). Client
  state: `~/.config/ranch/config.json` + `user.json` (0600). Env overrides:
  `RANCH_SUPABASE_URL`, `RANCH_ANON_KEY`, `RANCH_SOCKET`.
- Security: local socket 0600 same-uid only; machine key shown once at
  `ranch register`, only its SHA-256 is stored; terminal content transits
  Supabase unencrypted E2E in MVP (documented, accepted risk).
