# Ranch

A personal terminal multiplexer with a cloud relay: long-lived terminal
sessions on your machine, reachable from a local terminal (Linux-first),
a mobile app, and — by design — from anywhere later.

Sessions keep running while you're away. Reconnect from any device and
pick up exactly where you left off. tmux-style multiplexing is the core
(a tmux *replacement*, not a companion); **Forge** agent sessions and
**Mule** workflows are first-class pane types.

## Status

**Working today** (tested):

- **Local multiplexing core** — `ranchd` daemon owns sessions/panes
  (PTY + server-side terminal emulation via libghostty-vt); `ranch`
  CLI attaches, renders, and sends input over a local unix socket.
  Sessions survive client disconnect; resize reflows; scrollback
  (heuristic ring, 2000 lines).
- **tmux keybindings** — `Ctrl-B` prefix: `d` detach, `n`/`p` session
  nav, `o`/`l` pane cycle, `c` new session, `&` kill session,
  `%`/`"` split, `x` kill pane, `s` session picker, `,` rename,
  `:` command prompt, `Ctrl-B Ctrl-B` literal passthrough.
  `Ctrl-C` goes to the shell like any other key.
- **Cloud relay (Supabase)** — private Realtime channel per machine,
  RLS-gated to the owner + the machine itself. Frames flow
  machine ⇄ cloud ⇄ remote client; session mirror + heartbeat let any
  device list machines/sessions even while the machine is offline.
  Local clients never touch the cloud (verified: zero frames leave the
  machine for local-only activity).
- **Multi-user identity** — Google OAuth sign-in (Supabase Auth) with
  browser PKCE flow (`ranch login`); self-serve machine registration
  (`ranch register`, no admin credentials on devices); multiple
  daemons per account; `ranch machines` / `ranch cloud` to list
  devices and sessions.
- **Packaging** — `make install` puts binaries in `~/.local/bin`;
  `make service` installs/starts a systemd user unit for the daemon.

**Not yet** (the actual multiplexing UI):

- Split panes are protocol-level only — the daemon tracks split trees,
  but the attach TUI still renders one full-screen pane at a time.
  Next: real split rendering, then a session-tree sidebar so `ranch`
  feels like one app over all machines/sessions.
- Mobile app (M3), Forge/Mule first-class panes (M4).
- Seq-gap auto-resync is protocol-supported; not yet client-wired.

## Quick start

```sh
make install          # builds + installs to ~/.local/bin
ranch login           # Google sign-in (once per device)
ranch register        # on a daemon host: register with your account
make service          # install + start the ranchd systemd user unit
ranch                 # interactive session manager: list/create/attach/kill
ranch new work        # or straight to the point: create + `ranch attach work`
ranch machines        # list your machines (any device)
ranch cloud           # list sessions across machines
```

Inside `ranch` (dashboard) or `attach`: `Ctrl-B` is the tmux-style
prefix — `%`/`"` split, arrows move focus, Ctrl-arrows resize,
`d` detaches. Sessions keep running while detached.

Keys inside `attach`: see `Ctrl-B` table above; the status bar flashes
`[prefix]` when the next key is a command.

## Layout

```
ranch/
├── Makefile            # build / install / service
├── docs/
│   ├── SPEC.md         # research, architecture, milestones (M0–M4)
│   └── PROTOCOL.md     # wire protocol (daemon ⇄ clients ⇄ relay)
├── crates/             # Rust workspace: protocol, daemon, cli, vt
├── supabase/migrations # schema + RLS (incl. self-serve register RPC)
├── tools/relay-test.mjs# raw Realtime test client
├── systemd/ranchd.service
└── mobile/             # Expo app (M3, not started)
```

## Docs

- `docs/SPEC.md` — architecture, security model, milestones M0–M4 with
  detailed completion notes (M0/M1 ✅, M2/M2.5/M2.6 ✅)
- `docs/PROTOCOL.md` — frame reference; same frames on the unix socket
  and the relay
