# Ranch

A personal terminal multiplexer with a cloud relay: long-lived terminal
sessions on your machine, reachable from a local terminal (Linux-first),
a mobile app, and — by design — from anywhere later.

Sessions keep running while you're away. Reconnect from any device and
pick up exactly where you left off. Simple tmux-style multiplexing is
the core; **Forge** agent sessions and **Mule** workflows are
first-class pane types.

## Status

Pre-code. Start with `docs/SPEC.md` (architecture + MVP scope) and
`docs/PROTOCOL.md` (wire protocol).

## Layout

```
ranch/
├── docs/
│   ├── SPEC.md         # research, architecture, MVP component specs
│   └── PROTOCOL.md     # wire protocol (daemon ⇄ clients ⇄ relay)
├── crates/             # Rust workspace (M1): protocol, daemon, CLI
└── mobile/             # Expo app (M3)
```
