# Cloud voice (Herd): off-lab phone → Supabase edge → your lab

Status: **end-to-end live over the public internet via an outbound
Cloudflare tunnel (2026-10-11).** The lab keeps **zero public ingress**:
no NAT forwards, no public A record, no inbound ports — a `cloudflared`
agent inside the voice-gw container dials OUT to Cloudflare, and the
public hostname terminates at the CF edge. Currently on a **quick
tunnel** (ephemeral `*.trycloudflare.com` URL — changes whenever the
container restarts); a **named tunnel** (one-time dashboard step) makes
it permanent.

> **Alternative design (parked):** Supabase Realtime broadcast as the
> relay — zero public address at all, no Cloudflare, lab keys leave
> Postgres; caps measured, build plan written. See
> `cloud-voice-relay.md`.

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
public tunnel hostname (quick: <rand>.trycloudflare.com; named: e.g.
voice-lab.jbutler.dev CNAME'd to the tunnel's *.cfargotunnel.com)
  ▼
Cloudflare edge (TLS, WebSockets-capable)
  │  ← the INBOUND leg of a dial the lab made OUTBOUND (cloudflared
  │    agent in the voice-gw container; the lab router sees no inbound
  │    connections at all — zero public ingress by policy)
  ▼
voice-gw ALC container on mini (two processes, image local/voice-gw-alc;
repo sources ct/voice-gw/Dockerfile + run.sh): cloudflared agent +
nginx allowlist on localhost:8443 (ct/voice-gw/nginx.conf): /voice/* →
mule :8091, /v1/audio/* → forge :8110, /api/v1/jobs +
/api/v1/workflows* → mule, everything else 404.
```

The edge function's `{forge_base}` / `{mule_base}` rows point at the same
tunnel hostname, so proxied STT/jobs/workflows cross the public internet
once (phone→Supabase) and again at Supabase→CF→lab — all outbound from
the lab's point of view. **On-lab fallback (unchanged, optional):**
`voice-lab.butler.ooo` internal DNS → NPM proxy host (id 79) → host
forward 8843→8443 still serves lab-network devices; off-lab uses the
tunnel hostname.

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
| `voice-gw` allowlist container + **outbound cloudflared tunnel agent** (two processes; image `local/voice-gw-alc` built from `ct/voice-gw/Dockerfile` + `run.sh`) | lab `ct/voice-gw/` + `hosts/mini/alc.toml` | **deployed + verified** (allowlist verified through the public URL: healthz 200 / deny 404 / mule 401→200 / WS reached mule) |
| Quick tunnel `https://catering-integral-andrew-yields.trycloudflare.com` (ephemeral) | voice-gw container; URL logged to `~/.lab-alc/data/voice-gw/tunnel.log` | **live now** — the test user's `voice_gateways` row points here |
| Named tunnel (permanent hostname, e.g. `voice-lab.jbutler.dev` → service `http://localhost:8443`; `VOICE_GW_TUNNEL_TOKEN` in the pool env) | CF dashboard + pool env | **optional next step** |
| NPM proxy host `voice-lab.butler.ooo` + internal DNS record + host forward 8843→8443 | NPM id 79 / lab Technitium / forwarder | **live, now optional** (on-lab fallback path) |
| Google OAuth provider on the Supabase project | Google Cloud client + `POST /auth/providers/google` (sbp_ token) | **pending (needs the OAuth client)** |

> **Why a tunnel, not a public A record:** policy is zero public ingress
> to the lab — no NAT forwards, no public IP exposure. `voice-lab.
> butler.ooo` exists only in the lab-internal Technitium zone (public DoH
> shows NXDOMAIN), so the Supabase edge could not resolve it; the
> outbound Cloudflare tunnel is the fix: the lab's only connection to
> the public internet is cloudflared's outbound 443 dial.

## Test record (2026-10-11)

- `GET /health` → 200; no token / garbage token → 401 (all routes).
- Test user `voice-gw-test2@jbutler.dev` (admin-confirmed via a
  since-removed token-gated hook): password grant → user JWT.
- `rpc/upsert_voice_gateway` with that JWT → 204; RLS read-back returns the
  row; row first pointed at public-echo stubs (httpbingo/postman-echo) to
  prove forwarding plumbing, then re-pointed at the tunnel hostname.
- STT: multipart body forwarded intact (boundary + content-length echoed by
  the stub); jobs: `Authorization: Bearer <row key>` injected (echoed);
  workflows: forwarded.
- **Through the tunnel (final, off-lab):** healthz 200 / deny 404 / mule
  401-no-key / mule workflows 200-with-key / WS upgrade reaches mule (not
  nginx-404); edge fn → tunnel → mule workflows 200; edge fn → tunnel →
  forge TTS hits real forge validation (422 on a malformed probe — i.e.
  the request reached forge, not a 502).

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

1. **Optional — permanent hostname (named tunnel):** CF dashboard → Zero
   Trust → Networks → Tunnels → New tunnel (Cloudflared connector) → copy
   the token → lab: put `VOICE_GW_TUNNEL_TOKEN=<token>` in the pool env
   (`~/.lab-alc/data/voice-gw/env`, sourced by `run.sh`) → dashboard:
   public hostname `voice-lab.jbutler.dev` → service
   `http://localhost:8443` → restart the voice-gw container → re-point
   the `voice_gateways` row (phone Settings → "Save cloud gateway", or
   the RPC). Until then the quick-tunnel URL stands — it **changes on
   every container restart** (read `~/.lab-alc/data/voice-gw/tunnel.log`
   and update the row).
2. Phone: sign in, Settings → enable "Cloud voice", CG fields =
   `https://catering-integral-andrew-yields.trycloudflare.com` (today)
   + the two lab keys, "Save cloud gateway", then PTT + 📞 off-lab.
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
  instead of dying. Also added: **container image-mismatch detection**
  (name-based adoption otherwise keeps starting a container whose image
  the manifest no longer names — observed with the voice-gw
  nginx:alpine → local/voice-gw-alc move).
- **ALC CLI auth is per-user**: `sudo container …` write ops fail with
  "unauthorized request" (root's apiserver ≠ the user's containers);
  use the plain CLI as the user (reconcile's `alc()` helper does
  `launchctl asuser 501 su jbutler`). A macOS ALC runtime update
  (2026-10-11 14:30) briefly put the apiserver in exactly this state.
- **NPM proxy host**: `forward_scheme` must stay `http` (NPM terminates
  TLS; the upstream is plain). UI default is ON — flip it off, or the
  documented sqlite edit (`update proxy_host set forward_scheme='http'
  where id=…` + regenerate/reload).
- **`container run` (foreground) blocks** the session at "Starting
  container" even when the container came up fine — always use `-d`;
  a killed foreground run can still leave the container created+running
  (clean it up before the next named create).
