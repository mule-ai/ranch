# Design: Webhook Receiver (Supabase Relay)

*External services start work on a ranch machine through an
authenticated, rate-limited edge function that forwards into the
per-machine Realtime channel the daemon already listens on.*

Status: design (Phase E of PLAN.md).

## 1. Shape

```
 GitHub / CI / mule / forge / anything
        │
        │ POST https://<proj>.supabase.co/functions/v1/webhook-relay/w/<webhook_id>
        │ X-Ranch-Signature: t=1757680000,v1=<hex(hmac_sha256(secret, body))>
        ▼
┌─────────────────────────────┐
│ edge function: webhook-relay│
│ 1. webhook_id → row         │
│    (service role lookup)    │
│ 2. HMAC + freshness check   │
│ 3. rate limit (token bkt)   │
│ 4. size cap (64 KB)         │
│ 5. broadcast WebhookEvent ──┼──▶ realtime:machines:<machine_id>
│ 6. webhook_log insert       │        │
└─────────────────────────────┘        │ (existing relay WS path)
                                       ▼
                              ┌────────────────┐
                              │ ranchd         │
                              │ relay pipe →   │
                              │ handle_ws_text │──▶ triggers.rs
                              │ (webhooks.rs)  │    (match → run workflow)
                              └────────────────┘
```

Key property: **no new inbound path to the daemon.** The edge function
is a sender on the same private channel the daemon already consumes;
the daemon treats `WebhookEvent` frames exactly like any other relay
frame (arriving via `handle_ws_text`, `relay.rs:557`). No ports
opened, no daemon-side listener.

## 2. Multi-user addressing

One webhook id per (machine, purpose). The id is a UUID (unguessable)
and the secret is required — so the URL alone can't fire anything.

- `webhooks` rows are owned by `user_id` (RLS: owner-only), so user X
  can never enumerate or target user Y's webhooks even with the
  project URL.
- The edge function resolves `webhook_id → machine_id` with the
  service role (webhook rows are not world-readable); it broadcasts
  only to that machine's channel.
- A webhook can target any machine the owner registered (multi-machine
  accounts pick the machine at webhook creation).

## 3. Signature scheme

GitHub-style, dated, and versioned — familiar to integrators:

```
X-Ranch-Signature: t=<unix-seconds>,v1=<hex>
v1 = HMAC-SHA256(secret, "<t>.<raw-body>")
```

- Freshness: reject `t` older than 300 s or in the future by >60 s
  (replay window).
- Compare with a constant-time equality check.
- Secret: 32 random bytes, base64url; shown **once** at creation;
  stored as SHA-256 in `webhooks.secret_hash` (same one-time-display
  pattern as the machine key).
- Native GitHub sources: also verify `X-Hub-Signature-256` against the
  webhook's stored GitHub secret (a `source_kind = "github"` row) —
  same table, different verification branch.

## 4. Frame

```jsonc
{
  "t": "WebhookEvent",
  "webhook": "7c0e…",             // webhook id
  "source": "github",             // source tag (free-form, matched by triggers)
  "event": "push",                // event tag (optional)
  "payload": { "…": "…" },        // verified JSON body, ≤64 KB
  "received_at": "2026-09-12T…Z"
}
```

Non-JSON bodies: forwarded base64 with `"encoding":"base64"` — the
daemon logs them but triggers only match JSON.

## 5. Rate limiting

Per-webhook token bucket, capacity 10, refill 1/s (configurable on the
row: `rate_limit_per_min`). Implementation: the function keeps counts
in a `webhook_log` window query (simple, no shared state infra) —
`select count(*) from webhook_log where webhook_id = $1 and
received_at > now() - '60 seconds'` before broadcasting; over budget →
429 + log row with `status=429`. Cheap enough at personal scale; swap
for Deno KV if it ever isn't.

## 6. Failure semantics

| failure | response | daemon effect |
|---|---|---|
| unknown id | 404 | — |
| bad/stale signature | 401 | — |
| over rate limit | 429 | — |
| body > 64 KB | 413 | — |
| machine offline | 202 | frame lands in the Realtime channel; daemon picks it up on reconnect **if** Realtime retains (it does not guarantee) — otherwise lost. Documented: webhooks are best-effort for offline machines; the `webhook_log` row records what was sent. A "pending deliveries" replay (daemon pulls missed log rows on boot via REST) is a designed follow-up. |
| trigger matches, run fails | 202 | normal workflow failure surfaces in the workflow pane + job status. |

## 7. What the receiver can and cannot do

Can: emit `WebhookEvent` frames to a machine channel, write
`webhook_log`.

Cannot: read terminal frames (it never subscribes), join other
machines' channels (routing is fixed by the webhook row), execute
workflows directly (that's the daemon's trigger evaluation), or mint
tokens. Service-role scope is deliberate and auditable; the function
is ~150 lines of Deno with no secrets beyond the service key it
already runs with.

## 8. Client surface (all surfaces)

Webhooks screen (web + mobile + TUI overlay):

- list: name, machine, sources, last-received (from `webhook_log`),
  enabled;
- create: name, machine picker, source tags, rate limit; **one-time
  secret + URL reveal** with copy button and a ready-made signed
  `curl` example;
- test-fire: "send test event" button (client → daemon → it's just a
  REST call to the function with a daemon-generated signature, so the
  round trip is verifiable from the UI).
