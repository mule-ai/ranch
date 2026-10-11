# Cloud voice (Herd): off-lab phone → Supabase edge → your lab

Status: **edge gateway live + tested (2026-10-11); lab public surface pending
(NPM proxy host + `ct/voice-gw` allowlist container + DNS record).**

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
| `voice-gw` nginx allowlist container + host forward 8843→8443 | lab `ct/voice-gw/` + `hosts/mini/alc.toml` | **committed, not deployed** (needs image pull + reconcile) |
| NPM proxy host `voice-lab.butler.ooo` → `10.10.199.29:8843` (Force SSL, WebSockets on) | NPM admin (:81) | **pending (dashboard)** |
| Public DNS A record `voice-lab.butler.ooo` → lab router public IP | public butler.ooo zone | **pending (dashboard)** |
| Google OAuth provider on the Supabase project | Google Cloud client + `POST /auth/providers/google` (sbp_ token) | **pending (needs the OAuth client)** |

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

1. NPM: new proxy host `voice-lab.butler.ooo` → `10.10.199.29:8843`,
   Force SSL, WebSockets on.
2. Public DNS: A record `voice-lab.butler.ooo` → the lab router's public
   IP (same target as the other *.butler.ooo records).
3. Mini: `mkdir -p ~/.lab-alc/data/voice-gw && cp ct/voice-gw/nginx.conf
   ~/.lab-alc/data/voice-gw/default.conf && sudo container image pull
   nginx:alpine && sudo lab-alc-reconcile`; verify
   `curl http://10.10.199.29:8843/healthz` → `ok` and
   `curl -o /dev/null -w '%{http_code}' http://10.10.199.29:8843/api/v1/agents`
   → `404`.
4. Phone: sign in (password now; Google once enabled), Settings → enable
   "Cloud voice", set the CG fields to `https://voice-lab.butler.ooo` +
   the two keys, "Save cloud gateway", then PTT + 📞 off-lab.
5. Google: create an OAuth client (redirect `ranch://auth-callback`), then
   enable the provider via the Supabase mgmt API (runbook in
   `docs/LAB-SECRETS.md`).
