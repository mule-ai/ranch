# Cloud voice v2: Supabase Realtime as the relay (the no-tunnel alternative)

Status: **proposal / parked.** The live design is
`cloud-voice.md` (Supabase edge fn + outbound Cloudflare tunnel +
allowlist nginx; quick tunnel live 2026-10-11). This document specifies
the alternative that was raised against it: **Supabase Realtime broadcast
channels as the relay for voice traffic**, so that the lab has no
public address of any kind and no audio transits a pipe we don't run.
Revisit when the basics (tunnel design) are working and we decide which
plane owns voice.

## Constraints this design targets

1. **Zero public ingress to the lab** — stronger than the tunnel design
   (which has none at the router, but does expose a public hostname via
   the Cloudflare edge).
2. **Supabase is the only third party** — the Herd already trusts it
   with auth, relay frames, and (today) gateway keys; voice stays inside
   that boundary instead of adding Cloudflare as an audio pipe.
3. **Both ends dial out** — phone → Supabase (wss), lab daemon →
   Supabase (wss). Supabase relays. Nothing listens on the lab.

## Why this is not the rejected "Postgres bus"

An earlier alternative — using Realtime **postgres-changes** (CDC) as the
message bus — was rejected on per-frame commit latency, WAL retention of
audio frames, and sustained insert rates. That critique does **not**
apply to **broadcast** channels: frames ride an already-established
WebSocket with no per-frame round-trip or commit, and nothing is
persisted. The two Realtime mechanisms have very different properties;
this design uses only broadcast.

## What is already proven

The ranch relay **is this design, in production**: the phone broadcasts
frames on the private `realtime:machines:<id>` channel; ranchd's
`relay.rs` holds its own outbound wss to Supabase and receives them;
both directions, sustained ~33 frames/sec per client (30 ms
coalescing tick). Voice would be new frame families on a channel both
ends already hold open — no new transport, no new connection.

## Measured caps (2026-10-11, on `prqfseydoxyingbkmiic`)

Broadcast payload ladder (client A sends, client B receives, one public
`realtime:` channel; scratch script, to be committed as
`tools/realtime-cap-test.mjs` when this is picked up):

| Sent payload | Result |
|---|---|
| 98 KB | delivered |
| **250 KB** | **delivered** |
| 500 KB | silently dropped (sockets survive) |
| 1 MB / 2 MB / 4 MB | silently dropped |
| 10 MB | sender socket **killed, code 1009** ("message too big") |

Consequences:

- **Call loop:** 25 ms of 64 kbps AAC ≈ 200 B raw ≈ 270 B base64 →
  40 msg/s per direction fits with ~900× headroom. No chunking.
- **PTT STT:** ~10 s utterance ≈ 107 KB base64 → one message. ~30 s ≈
  320 KB → needs the `seq/fin` chunk protocol below. Chunk size:
  **240 KB** (safety margin under 250).
- **Rate:** 40 msg/s/dir is the same order the relay already sustains;
  not separately tested (low risk).
- Also learned: Realtime topics must be `realtime:`-prefixed
  (plain names → `unmatched topic` on join).

## Architecture

```
phone (mobile-native)                     lab
  │  voice frames as Realtime               │
  │  broadcast on the machine channel       │
  ▼                                          │
Supabase Realtime (the relay; both sides    │
dial OUT — zero inbound to the lab)         │
  │                                          ▼
  └──────────────────►  ranchd voicebridge worker
                           │  (new worker; standard pipe-pair/mpsc
                           │   pattern; forks relay's channel join)
                           │
                           ├─ VoiceJob      → POST {mule_url}/api/v1/jobs (LAN, local key)
                           ├─ VoiceAudio    ⇄ local wss /voice/{job}/in|out (mule, UNCHANGED)
                           ├─ VoiceStt*     → assemble → POST {forge_url}/v1/audio/transcriptions (LAN)
                           └─ VoiceText     → mule in-socket text frame (the CLI typed-utterance path)
```

- **Channel:** reuse the existing `realtime:machines:<machine_id>`
  private channel (the relay channel). It already has: RLS gating via
  `realtime.messages` (migration 0003), phone-side subscription
  (mobile-native keeps it open for relay frames), daemon-side
  subscription (`relay.rs`), seq tracking + gap detection on the phone.
  A new parallel channel (`realtime:voice:<uid>`) is an option but buys
  nothing in v1 and doubles the auth plumbing.
- **Mule: unchanged.** The voicebridge is a LAN client of mule exactly
  like the phone's `CallActivity` is today (same `/api/v1/jobs` POST,
  same `/voice/{job}/{in,out}` sockets, same control frames
  `{"hangup":true}` / `{"flush":true}`).
- **Forge: unchanged.** STT/TTS reached locally via `daemon.toml` keys
  (the daemon already holds both — `forge_url`/`forge_api_key`,
  `mule_url` + admin key).
- **`voice_gateways` table + `ranch-voice` edge function:** become
  vestigial for cloud voice. The lab's forge/mule keys **leave Supabase
  Postgres entirely** (the key columns were only ever read by the edge
  fn). The machine channel's RLS becomes the whole access boundary.
  Keep the table dormant (don't drop) until v2 is live.
- **Current tunnel design: kept as fallback** (web/Expo path, LAN, and
  the escape hatch during cutover) until the relay path is proven
  end-to-end off-lab; then it can go dark.

## Frame protocol (new families in `ranch-protocol`, JSON, on the relay channel)

Phone → daemon:

| Frame | Fields | Notes |
|---|---|---|
| `VoiceJobRequest` | `name` (workflow, e.g. `voice-call`) | bridge looks it up locally, POSTs mule |
| `VoiceJobOk` | `job` (id), `name` | bridge → phone |
| `VoiceAudio` | `job`, `dir` (`in`), `b64` (≤ 240 KB) | one 20–50 ms chunk |
| `VoiceStt` | `req` (client id), `seq`, `fin`, `b64` (≤ 240 KB) | PTT upload; 10 MB assembly cap |
| `VoiceSttDone` | `req` | (result arrives as `VoiceSttResult`) |
| `VoiceSttResult` | `req`, `text`, `err?` | bridge → phone |
| `VoiceText` | `job`, `text` | typed-utterance fallback → mule in-socket text frame |
| `VoiceHangup` | `job` | bridge closes both local sockets; mule's hang-up path terminates the call |

Daemon → phone also reuses the existing `Meta`/`ActivityPush` families
for call events (turn start/end, error rows) — no new notification
plumbing (mobile notification engines already key off `⚠` rows and
`turn_end` for agent panes; voice-call events map to the same shapes).

## Daemon: `voicebridge` worker (new file `crates/ranch/src/voicebridge.rs`)

- Worker-thread pattern (PLAN.md): own pipe pair into the poll loop, jobs
  via `mpsc`, frames broadcast to clients like any other client frame.
  Restart-safe across `ranch upgrade` (Tier-2 hot upgrade: threads die
  with the exec and restart — an in-flight call drops, see
  "Reliability" below).
- Opens its **own** Realtime wss and joins
  `realtime:machines:<id>` with the machine key (independent of
  `relay.rs` — two subscriptions on one channel each receive their own
  copy; no coupling, no shared state).
- Audio pump: while a call job is open, two local WS connections to
  mule; Realtime `VoiceAudio` → binary frame on the in-socket;
  out-socket binary frame → `VoiceAudio` broadcast. No buffering beyond
  the socket buffers (backpressure: drop oldest, count drops — voice is
  loss-tolerant, see Reliability).
- STT assembler: map `req` → chunks; `fin` → POST forge
  `/v1/audio/transcriptions` (multipart, local key) → `VoiceSttResult`.
  10 MB cap; stale `req` entries expire after 60 s.
- Uses only `daemon.toml` (`mule_url`, `mule` admin key, `forge_url`,
  `forge_api_key`). No Supabase service-role key; no `voice_gateways`
  reads.

## Phone (mobile-native): `RealtimeTransport`

- Voice.kt gains a transport interface: the current call path is
  direct-WS (`wss://{base}/voice/{job}/…`); add a `RealtimeTransport`
  that sends/receives the `Voice*` frames on the Realtime socket the
  app already keeps open (MonitorService / relay frames). PTT gains a
  `VoiceStt` chunked send instead of the edge-fn/forge POST.
- `CallActivity` selects transport by the "Cloud voice" setting:
  relay path (this design) vs direct path (current tunnel design). The
  two are interchangeable from the call loop's point of view (both feed
  mule's in-socket semantics).
- Seq-gap detection for voice frames: reuse the phone's existing
  relay-frame gap logic; a voice gap is a "choppy audio" indicator, not
  a resync trigger (audio is not replayable).

## Reliability & failure modes (documented, accepted)

- **Transport blip / frame loss:** broadcast is fire-and-forget. A lost
  audio chunk = a glitch in the call, like a dropped WebRTC packet (the
  design's ancestor, telephony, is lossy by design). Lost control frames
  (`VoiceHangup`, `VoiceJobRequest`): the 30 s Realtime heartbeat
  detects a dead transport; the call tears down when either end's
  socket closes (same as today's mule WS: socket close ends the call).
- **Daemon restart / hot upgrade:** in-flight calls drop; the phone
  detects it via socket-dead (relay channel) or heartbeat gap and shows
  "call ended". Equivalent to the current design's container-restart
  failure mode.
- **base64 overhead:** +33 % on audio — irrelevant at voice rates
  (~1 KB/s per direction after overhead).
- **PTT latency:** +1–3 s vs the tunnel path (chunking + reassembly +
  one extra service hop). PTT is already an "upload, then wait" UX.

## Security model (delta vs the tunnel design)

- **Access boundary = the machine channel's RLS** (migration 0003):
  only credentials that can join `realtime:machines:<id>` can send
  `Voice*` frames. That's the same boundary that already exposes
  terminal frames — voice adds no new surface class. (A compromised
  phone with valid credentials already has the terminal.)
- **Lab keys never touch Supabase Postgres** (delta from the live
  design, where `voice_gateways` stores the forge/mule keys RLS-protected
  but still in the cloud DB).
- **Mule admin API + forge API stay LAN-only**; the bridge is their only
  non-phone client and it's local.
- **Job ids** remain capability tokens inside a call; the channel is the
  outer boundary.
- **Multi-user:** v1 = machine owner only (existing RLS). v2 = a
  `machine_members` table + RLS extension (same pattern as the
  `register_machine` self-serve RPC) when a second phone on one lab
  becomes a requirement.

## What gets retired (when v2 is proven)

- `ranch-voice` edge function (or keep for the Expo/web parity path only).
- `voice_gateways` key columns (table stays dormant).
- Phone CG settings fields + "Save cloud gateway" (cloud mode becomes a
  one-line toggle: relay vs direct).
- The Cloudflare tunnel + `voice-gw` container **can** be retired, but
  the allowlist container is cheap to keep as the web/Expo + LAN path —
  decision at cutover.

## Build order (when picked up)

1. **Protocol:** `Voice*` frame families in `ranch-protocol` (data +
   tests; no I/O).
2. **Daemon:** `voicebridge.rs` worker + config gate (enabled by the
   presence of `mule_url`/`forge_url` in `daemon.toml` + a
   `voice_relay = true` flag) + loopback test: `tools/relay-test.mjs`
   (or its expanded sibling) as the fake phone on the real channel;
   bridge verified against **local** mule+forge (scratch Postgres
   setup), and against lab mule/forge on-LAN via `VoiceText`
   typed-utterance frames (no real audio needed to prove the loop).
3. **Phone:** `RealtimeTransport` + PTT chunking + `CallActivity`
   toggle; on-lab phone smoke first, then off-lab.
4. **Cutover:** flip the default to relay; keep the tunnel path as the
   escape hatch; decide the tunnel/`voice-gw` fate; update
   `cloud-voice.md` status, `LAB-SECRETS.md`, and the VERIFY-HERD cloud
   section.

## Risks / open questions

- **Supabase Realtime message-rate limits on the project's plan** are
  not documented publicly; the relay already runs ~33 fps and a call
  adds ~80 msg/s — monitor for 413/1008-class disconnects during the
  first real calls.
- **256 KB broadcast cap is an observed platform constant**, not a
  contract; re-run the cap test after Supabase Realtime upgrades
  (commit the probe script to `tools/` first).
- **Multi-user on one lab** is v2 (owner-only RLS today).
- **Phone battery/foreground:** the Realtime socket is already kept
  alive by MonitorService's foreground service — no new Android
  plumbing (this is a genuine advantage over the tunnel path, where the
  call's wss needs the CallActivity's lifecycle).

## Cross-references

- Live design + status: `docs/design/cloud-voice.md`
- Wire frames: `docs/PROTOCOL.md` (relay frame family; `Voice*` to be
  added under §8-style "planned families" when work starts)
- Relay mechanics + gotchas: `AGENTS.md` (Realtime join-before-send,
  RLS via `realtime.messages`, worker pattern)
- Lab tunnel/allowlist (the fallback): `~/src/lab/ct/voice-gw/README.md`
