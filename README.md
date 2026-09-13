# ranch 🤠

A workbench for humans and agents, built on a terminal multiplexer.
Long-lived sessions on your own machine — shell panes, AI agents, and
agent workflows — reachable from your terminal, any browser, or your
phone.

**[⚡ Try the live demo](https://mule-ai.github.io/ranch/#/app)** — no
sign-up; one tap runs a sandboxed shell and an agent in a public demo
instance.

![ranch web app](docs/screenshot-web-landing.png)

Sessions keep running while you're away. Reconnect from any device and
pick up exactly where you left off — including agent conversations.
tmux-style multiplexing is the core (a tmux *replacement*, not a
companion); agents and workflows are first-class pane types that render
as conversations and live run timelines, not raw terminal dumps.

## Install

**Linux x86_64** — one static binary, no runtime dependencies:

```sh
curl -fsSL https://raw.githubusercontent.com/mule-ai/ranch-dist/main/ranch-linux-x86_64.tar.gz | tar xz
cd ranch-linux-x86_64
install ranch ~/.local/bin/ranch
ln -sf ranch ~/.local/bin/ranchd
```

Or grab the tarball / signed **Android APK** from
[**Releases**](https://github.com/mule-ai/ranch/releases/latest) or the
[download page](https://mule-ai.github.io/ranch/#/download).

<details>
<summary>Build from source</summary>

```sh
git clone https://github.com/mule-ai/ranch && cd ranch
make setup        # one-time: Zig 0.16 + pinned ghostty (for libghostty-vt)
make install      # ~/.local/bin/ranch (+ ranchd symlink)
make service      # systemd user unit for the daemon (optional)
```

Requires Rust (stable) + Zig 0.16; JDK 17 + the Android SDK only for
building the APK locally.

</details>

## Quick start

> **No install needed to kick the tires:** the [live
demo](https://mule-ai.github.io/ranch/#/app) signs you in to a shared
demo machine — a QuickJS sandbox shell (no fs, no net) and a no-tools
forge agent.

```sh
ranch login           # Google sign-in (once per device)
ranch register        # on a daemon host: register it with your account
ranch daemon          # run the daemon (or the systemd unit from `make service`)
ranch                 # interactive session manager
ranch new work        # create a session + attach
ranch machines        # list your machines (from any device)
ranch cloud           # list sessions across machines
```

Then open [the web app](https://mule-ai.github.io/ranch/#/app) or the
Android app: same sessions, same machines, live terminal, from anywhere.

`Ctrl-B` inside `ranch` is the tmux-style prefix: `%`/`"` split, arrows
move focus, `Ctrl`-arrows resize, `d` detaches, `c`/`s`/`,`,`&` manage
sessions. Sessions keep running while detached — and while your laptop
is closed.

## What you can do

### Multiplex everything

Shell panes are the foundation: real PTYs, server-side terminal
emulation (Ghostty's engine), splits/windows/scrollback, tmux keys.
Attach from the TUI, the browser, or the phone — the screen is exactly
where you left it, because the daemon owns one canonical terminal per
pane and every client just renders it.

### Edit files from anywhere

Every surface has a file browser, editor, and markdown review for files
on the machine — thin clients over the daemon, with atomic writes,
mtime conflict detection ("changed on disk" banner, never a silent
clobber), and push notifications when watched files change externally.
The TUI hands big edits to your `$EDITOR` in a split; quick fixes are
inline. **- todo** for the TUI inline editor.

### Build agents without leaving ranch

Agent panes come in two flavors, identical UX: **forge**-backed
(durable sessions in forge's Postgres, resume from any device, live
SSE streaming) or **local `pi`** (zero infra, RPC child of the daemon,
session file survives hot upgrades). Chat bubbles, typing indicator,
tool-call chips with tap-to-expand output, and a live model picker on
every surface.

The **agent builder** — create and edit agent profiles (provider,
model, system prompt, tools, working dir) with a form on any surface,
and launch them as panes. **- todo** (design: docs/design/agent-builder.md)

### Let agents run workflows

**Mule** workflows are first-class: browse, edit, and run them from any
client, and watch the run stream live into a pane — step-by-step
timeline, agent output, tool calls — attachable from your phone.
**- todo**

**Triggers** run workflows without you: cron schedules (timezone-aware)
and event triggers — "when workflow X completes, run Y", "when an agent
finishes its turn", "when this file changes" — with run history on the
dashboard. **- todo**

**Webhooks** connect the outside world: a signed, rate-limited webhook
receiver routes external events (GitHub, CI, anything that can POST)
to your machines and fires workflows. **- todo** (design:
docs/design/webhook-receiver.md)

### Ranch is for agents, too

Agents running in ranch panes get tools to **spawn, steer, read, and
close sub-agent panes** — orchestration you can see, because every
sub-agent is a real pane in your layout. The parent agent gets a
callback when a sub-agent finishes its turn (the result is delivered to
the caller and rendered as a row in its pane); you can watch, interject
in, or close any sub-agent pane by hand at any time. Spawn policy is
yours: allow, deny, or ask (approval chips on every attached client).
Works for both runtimes — local-pi agents call the daemon directly;
forge agents (usually on another host) reach it through a forge-hosted
bridge, so a forge agent can spawn panes right beside your shells.
**- todo** (design: docs/design/agent-tools.md)

## How it fits together

```
 local TUI ─┐                                      ┌─ phone (Expo)
 browser ───┼─ Supabase Realtime ── ranch daemon ──┼─ PTY panes (libghostty-vt)
 ranch CLI ─┘    (RLS-gated relay)                ├─ agent children (pi, forge)
 external ─────webhooks──── edge function ────────┴─ mule workflow runs
```

- **One binary** — `ranch` is the CLI/TUI *and* the daemon (`ranch
  daemon`, the `ranchd` symlink, or `--daemon`). Fully statically
  linked (musl + the ghostty VT engine compiled to a static archive):
  ~21 MB, zero runtime deps.
- **The daemon owns everything** — one server-side terminal emulator
  per pane, 30 ms coalesced output, sessions/panes/layout in
  `state.json`; worker threads for forge (SSE), local pi (RPC), mule
  (REST + WS), and the trigger scheduler.
- **Sessions survive (almost) everything.** Two tiers:
  - *Tier 1* — daemon restart kills panes? `state.json` rebuilds
    sessions, panes, and agent conversations from disk on boot.
  - *Tier 2* — `ranch upgrade` hot-upgrades the daemon in place: it
    re-execs itself with `--inherit`, passing every PTY/agent/listener
    fd through the exec. Same PID, children never notice, zero dead
    panes.
- **Cloud relay** — a private Supabase Realtime channel per machine,
  RLS-gated to the owner + the machine itself. Terminal frames flow
  machine ⇄ cloud ⇄ remote client; heartbeats power the online
  indicator. Local clients never touch the cloud.
- **Multi-device identity** — Google OAuth sign-in; self-serve machine
  registration (`ranch register`, no admin credentials on devices);
  any number of daemons per account.

## The herd

| tool | role |
|---|---|
| **ranch** (this repo) | multiplexer + daemon + TUI/web/mobile clients |
| [**forge**](https://github.com/jbutlerdev/forge) | durable agent runtime: long-lived pi-backed sessions, persisted conversation, durable resume, OpenAI-compatible API |
| **pi** ([pi-mono](https://github.com/badlogic/pi-mono)) | local agent runtime: RPC mode drives agent panes and mule's workflow agents alike |
| **mule** ([mule-ai/mule](https://github.com/mule-ai/mule)) | workflow orchestration: multi-step agent + wasm workflows, jobs, WS event hub |

## Security model

- Machine identity is a dedicated Supabase Auth user per machine; its
  key is shown once at `ranch register` and stored `0600` on the host.
- Row Level Security everywhere: owners see only their own machines
  and sessions; realtime channels are joinable only by the owning user
  and the machine itself.
- The relay relays opaque frames — the cloud never sees terminal
  contents in plaintext at rest (frames live in-transit only).
- Forge/mule credentials live only in the daemon's `0600` config;
  clients never hold them. Agent-builder secrets are write-only.
- Webhooks are HMAC-signed with per-webhook secrets (shown once),
  timestamp-fresh, rate-limited, and can only emit events — never
  drive panes.

## Development

```
crates/ranch         the binary: client (client.rs) + daemon (daemon.rs,
                     forge.rs, pilocal.rs, relay.rs)
crates/ranch-protocol  wire protocol (same frames on socket + relay)
crates/ranch-vt        server-side terminal emulation (libghostty-vt)
web/               Vite + React web app (GitHub Pages)
mobile/            Expo Android app
supabase/migrations  schema + RLS policies
docs/SPEC.md       target-system spec
docs/PLAN.md       implementation plan (grounded in the code)
docs/PROTOCOL.md   frame reference
docs/design/       design docs: agent tools, webhooks, workflows
docs/history/      archived spec versions + milestone build log
```

```sh
make build         # static release binary
make test          # cargo test
npm --prefix web run dev
```

## Status

Working and tested: local multiplexing core (PTY + server-side
emulation, scrollback, resize reflow, tmux keys), attach TUI with split
panes and windows, cloud relay + multi-device apps (web + Android),
agent panes (forge + local pi) with chat bubbles and model picking,
file editing (mobile + web), Tier-1 session restore, Tier-2
zero-downtime hot upgrades, CI-published static binaries + signed APK.

Coming soon: the agent tool surface (spawn/steer/close sub-agents),
the agent builder, mule workflow panes + CRUD, cron/event triggers,
and the webhook receiver — spec'd in [docs/SPEC.md](docs/SPEC.md),
planned in [docs/PLAN.md](docs/PLAN.md).

## Docs

- [⚡ Live demo](https://mule-ai.github.io/ranch/#/app) — sandbox shell + agent, no sign-up
- [docs/SPEC.md](docs/SPEC.md) — what ranch is (target system)
- [docs/PLAN.md](docs/PLAN.md) — how we get there, milestone by milestone
- [docs/PROTOCOL.md](docs/PROTOCOL.md) — the wire protocol
- [docs/design/](docs/design/) — design docs + diagrams
- [Website](https://mule-ai.github.io/ranch/) · [Web app](https://mule-ai.github.io/ranch/#/app)
