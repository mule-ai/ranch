# Cloud voice (Herd): off-lab phone → Supabase edge → your lab

Status: **edge gateway live; lab allowlist + NPM proxy host live
(2026-10-11); pending: PUBLIC A record for voice-lab.butler.ooo
(only the lab-internal zone has it — the Supabase edge, and phones off
the lab network, need it in the public Cloudflare butler.ooo zone →
173.48.243.181).**

The goal: a phone that is NOT on the user's local network can still use the
user's voice stack (forge STT/TTS, mule voice-loop calls) with **Supabase
sign-in (Google or password) as the credential and the routing key**. The
lab's API keys never reach the phone; each user's requests go to the
endpoints recorded for THAT user, and no user can reach another user's.

## Architecture

```
phone (mobile-native)
  │  Supabase user JWT (Google/password login — Auth.kt)
  ▼
ranch-voice edge function (public Supabase edge)
  │  1. verify JWT via GoTrue /auth/v1/user (anon key; 60 s cache)
  │  2. read voice_gateways[uid] via PostgREST with the SAME user JWT
  │     (RLS: only the caller's own row — the service role is NOT used
  │     for this table)
  │  3. forward, injecting the row's keys:
  │       POST /v1/audio/{transcriptions,speech} → {forge_base}/v1/audio/…
  │       POST /api/v1/jobs                      → {mule_base}/api/v1/jobs
  │       GET  /api/v1/workflows[/{id}/steps]    → {mule_base}/api/v1/…
  │       GET  /gateway                          → {mule_base_url} (no key)
  │
  │  call audio (WebSockets ONLY — see platform limit below):
  ▼
phone dials wss://{mule_base_url}/voice/{job}/in|out   (TLS, public)
  ▼
voice-lab.butler.ooo  (public DNS + NPM ACME) → voice-gw ALC container
  nginx allowlist (ct/voice-gw/nginx.conf): /voice/* → mule :8091,
  /v1/audio/* → forge :8110, /api/v1/jobs + /api/v1/workflows* → mule,
  everything else 404.
```

### Why the call's WebSockets don't cross the edge function

Supabase edge **rejects WebSocket upgrade requests at the platform level**:
verified 2026-10-11 — a function that returns a plain `200 JSON` to an
`Upgrade: websocket` request still gets `502` from the gateway (HTTP/2 gives
`404`), so no server-side WS (WebSocketPair) can be served there. The call
therefore dials the user's public mule base (from `GET /gateway`) directly.
Those sockets are safe by mule's own design: `/voice/*` is outside mule's
auth middleware and the **128-bit job id is the capability** (mule
`cmd/api/server.go`, H6.3); the allowlist nginx exposes only `/voice/*` of
mule, over TLS, and the job id is never logged by the edge function.

### Multi-user model

Each user = one Supabase account + one `voice_gateways` row (their lab's
public base + that lab's forge/mule keys). User A's phone cannot read user
B's row (RLS), so user A cannot reach user B's lab. Users with the same lab
share the public surface but each carries their own Supabase token; the
lab-side API keys still gate the proxied API paths.

## Pieces

| Piece | Where | State |
|---|---|---|
| `voice_gateways` table + `upsert_voice_gateway` RPC (RLS, SECURITY INVOKER) | ranch-2 `supabase/migrations/0009_voice_gateways.sql` | **applied** |
| `ranch-voice` edge function (verify JWT → own row → forward; caps: 10 MB audio, 30 audio req/min, 10 jobs/min, 60 s uid cache) | ranch-2 `supabase/functions/ranch-voice/index.ts` | **deployed + tested** (health/401/STT multipart passthrough/jobs Bearer injection/gateway) |
| Phone cloud mode: PTT STT via edge fn; call setup (workflows list + job POST) via edge fn; call WS dials `GET /gateway` base; Settings toggle + "Save cloud gateway" (CG fields + RPC) | mobile-native `Voice.kt` (CloudVoice), `SessionActivity.kt`, `CallActivity.kt`, `SettingsActivity.kt` | **built (APK in CI release)** |
| `voice-gw` nginx allowlist container + host forward 8843→8443 | lab `ct/voice-gw/` + `hosts/mini/alc.toml` | **deployed + verified** (healthz 200 / deny 404 / mule+forge reached, via LAN and public NPM) |
| NPM proxy host `voice-lab.butler.ooo` → `10.10.199.29:8843` | NPM (proxy host id 79; `forward_scheme=http` — the forward is plain HTTP behind NPM's ACME TLS) | **live** (DNS-01 cert; TLS + proxy verified from the laptop on-lab) |
| Lab-internal DNS A record `voice-lab.butler.ooo → 10.10.199.28` | lab Technitium zone | **live** (phones on the lab resolve it) |
| **Public** DNS A record `voice-lab.butler.ooo → 173.48.243.181` | Cloudflare `butler.ooo` zone | **pending (the one remaining step)** |
| Google OAuth provider on the Supabase project | Google Cloud client + `POST /auth/providers/google` (sbp_ token) | **pending (needs the OAuth client)** |

> **Why the public record matters:** the Supabase edge function runs on the
> public internet and saw `NXDOMAIN` for voice-lab.butler.ooo (verified
> 2026-10-11 via Google DoH: the *.butler.ooo subdomains exist only in the
> lab-internal Technitium zone). Until the public A record exists, cloud
> mode's proxied paths fail DNS at the edge (direct on-lab mode is
> unaffected, and the call's wss:// dials work from phones on the lab).

## Test record (2026-10-11)

- `GET /health` → 200; no token / garbage token → 401 (all routes).
- Test user `voice-gw-test2@jbutler.dev` (admin-confirmed via a
  since-removed token-gated hook): password grant → user JWT.
- `rpc/upsert_voice_gateway` with that JWT → 204; RLS read-back returns the
  row; **row points at public-echo stubs (httpbingo/postman-echo)** so the
  forwarding plumbing was proven without lab exposure.
- STT: multipart body forwarded intact (boundary + content-length echoed by
  the stub); jobs: `Authorization: Bearer <row key>` injected (echoed);
  workflows: forwarded.
- WS upgrades: platform 502 even for a plain JSON response — documented
  limit, not a code bug.

## Security posture (documented decisions)

- **Edge function `verify_jwt: false`** (platform-level check off): the
  function verifies the user JWT itself (GoTrue) on every route except
  `/health`. Unauthenticated callers get 401 and never reach a forward.
  Same posture as the existing `webhook-relay` function.
- **Service role key**: available in the function env but used for NOTHING
  (the gateway row read is RLS with the caller's own JWT).
- **Lab allowlist**: only 5 path families public; the mule admin API, the
  forge API proper, `/ws`, and all web UIs 404 at the allowlist.
- **Job ids** are capability tokens: not logged anywhere; TLS end to end
  (NPM ACME + wss).
- **Caps** (edge function, in-memory per uid): 10 MB audio body, 30
  audio requests/min, 10 jobs/min — v1 numbers, tune in the function.

## Remaining manual steps (owner)

1. **Cloudflare `butler.ooo` zone: A record `voice-lab.butler.ooo →
   173.48.243.181`** (lab router public IP, same target the router's NAT
   443-forward already serves to NPM). Proxied or DNS-only both work
   (DNS-01 cert already issued); DNS-only keeps voice audio off CF's
   edge. Re-point if the home IP changes (dynamic IP — no DDNS on
   butler.ooo today).
2. Phone: sign in (password now; Google once enabled), Settings → enable
   "Cloud voice", set the CG fields to `https://voice-lab.butler.ooo` +
   the two keys, "Save cloud gateway", then PTT + 📞 off-lab.
3. Google: create an OAuth client (redirect `ranch://auth-callback`), then
   enable the provider via the Supabase mgmt API (runbook in
   `docs/LAB-SECRETS.md`).

## Ops learnings (2026-10-11 deploy)

- **ALC `image tag` clobber**: the reconciler's machine re-tag step ran
  for containers too and pointed `nginx:alpine` at the NixOS machine
  rootfs. Fixed: tag line gated on `wtype=machine`
  (scripts/mac-alc-reconcile.sh, pending host rebuild via
  `sudo darwin-rebuild switch --flake /var/lib/lab-gitops/repo#mini`).
- **ALC run flags**: `--entrypoint` needs an absolute path
  (`/usr/sbin/nginx`); args without an entrypoint are treated as the
  executable; `-m` is memory (mounts are `--mount type=bind,…`).
- **nginx as uid 101**: ALC rejects `-u root` for nginx:alpine
  (noPasswdEntries); main.nginx (not `*.conf`, so its own http include
  can't pick it up) moves pid + temp paths to /tmp. Scratch dirs must be
  single-level (/tmp/nginx-proxy, not /tmp/nginx-tmp/proxy — nginx won't
  mkdir the parent).
- **reconcile set -e**: a transient ALC API flap made `machine_exists`
  false-positive on mule; the unguarded recreate then aborted the whole
  loop. Both recreate branches now double-check after 2 s and WARN
  instead of dying.
- **NPM proxy host**: `forward_scheme` must stay `http` (NPM terminates
  TLS; the upstream is plain). UI default is ON — flip it off, or the
  documented sqlite edit (`update proxy_host set forward_scheme='http'
  where id=…` + regenerate/reload).
