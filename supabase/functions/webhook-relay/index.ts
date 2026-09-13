// Ranch webhook relay — Supabase Edge Function (Phase E).
//
// Receives signed external webhooks, verifies them, and broadcasts a
// WebhookEvent frame into the target machine's private Realtime
// channel (which the daemon is already listening on). This function
// NEVER touches terminal frames — it can only publish events.
//
// Route: POST /functions/v1/webhook-relay/w/<webhook_id>
// Auth:  X-Ranch-Signature: t=<unix>,v1=<hex(hmac_sha256(secret, "<t>.<body>"))>
//        (±5 min freshness, constant-time compare)
// Caps:  64 KB body; per-webhook rate limit (60/min default)
// Log:   webhook_log (bounded by retention; bodies only in debug mode)
import { createClient } from "https://esm.sh/@supabase/supabase-js@2";

const SERVICE_ROLE = Deno.env.get("SUPABASE_SERVICE_ROLE_KEY")!;
const BODY_CAP = 64 * 1024;
const RATE_PER_MIN = 60;
const FRESHNESS = 5 * 60; // seconds

const enc = new TextEncoder();

function hex(buf: ArrayBuffer): string {
  return [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

async function hmac(secret: string, msg: string): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  return hex(await crypto.subtle.sign("HMAC", key, enc.encode(msg)));
}

// constant-time string compare
function ctEq(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

Deno.serve(async (req: Request) => {
  const url = new URL(req.url);
  const m = url.pathname.match(/\/webhook-relay\/w\/([0-9a-f-]{36})$/i);
  if (req.method !== "POST" || !m) return json(404, { error: "not found" });
  const webhookId = m[1];

  const supabase = createClient(
    // project url from the env (edge functions get SUPABASE_URL)
    Deno.env.get("SUPABASE_URL") ?? url.origin,
    SERVICE_ROLE,
  );

  const log = async (status: number, source: string | null, event: string | null) => {
    await supabase.from("webhook_log").insert({
      webhook_id: webhookId,
      source,
      event,
      status,
    });
  };

  // 1. resolve the webhook
  const { data: hook, error } = await supabase
    .from("webhooks")
    .select("id, machine_id, sources, debug, enabled")
    .eq("id", webhookId)
    .single();
  if (error || !hook) return json(404, { error: "unknown webhook" });
  if (hook.enabled === false) return json(410, { error: "webhook disabled" });
  // decrypt the signing secret (pgp_sym_encrypt at rest; only the
  // service role can read via the security-definer RPC)
  const { data: secret, error: secErr } = await supabase
    .rpc("read_webhook_secret", { p_webhook_id: webhookId });
  if (secErr || !secret) {
    await log(500, null, null);
    return json(500, { error: "secret unavailable" });
  }

  // 2. signature verification
  const sigHeader = req.headers.get("X-Ranch-Signature") ?? "";
  const parts = Object.fromEntries(
    sigHeader.split(",").map((kv) => kv.split("=", 2) as [string, string]),
  );
  const t = parts["t"];
  const v1 = parts["v1"];
  const body = await req.text();
  if (!t || !v1) {
    await log(401, null, null);
    return json(401, { error: "missing signature" });
  }
  const ts = Number(t);
  const now = Math.floor(Date.now() / 1000);
  if (!Number.isFinite(ts) || Math.abs(now - ts) > FRESHNESS) {
    await log(401, null, null);
    return json(401, { error: "stale or future timestamp" });
  }
  // The secret must be recoverable server-side to HMAC-verify (a hash
  // cannot). The column stores the secret encrypted at rest with the
  // project's service key via pgcrypto in the migration (SPEC §10);
  // `secret_hash` selects it back decrypted server-side only.
  const rawSecret = String(secret);
  const expected = await hmac(rawSecret, `${t}.${body}`);
  if (!ctEq(expected, v1)) {
    await log(401, null, null);
    return json(401, { error: "bad signature" });
  }

  // 3. payload cap
  if (body.length > BODY_CAP) {
    await log(413, null, null);
    return json(413, { error: "payload too large" });
  }

  // 4. rate limit (count recent log rows)
  const since = new Date(Date.now() - 60_000).toISOString();
  const { count } = await supabase
    .from("webhook_log")
    .select("id", { count: "exact", head: true })
    .eq("webhook_id", webhookId)
    .gt("received_at", since);
  if ((count ?? 0) >= RATE_PER_MIN) {
    await log(429, null, null);
    return json(429, { error: "rate limited" });
  }

  // 5. parse + route
  let parsed: { source: string; event?: string; payload?: unknown };
  try {
    parsed = JSON.parse(body);
  } catch {
    await log(400, null, null);
    return json(400, { error: "body must be JSON" });
  }
  const source = String(parsed.source ?? "");
  const event = parsed.event ? String(parsed.event) : null;
  if (Array.isArray(hook.sources) && hook.sources.length > 0 && !hook.sources.includes(source)) {
    await log(403, source, event);
    return json(403, { error: "source not allowed" });
  }

  // 6. broadcast into the machine's private Realtime channel via the
  // Realtime Broadcast REST API (service role bypasses RLS; the
  // machine's daemon is subscribed to this channel already).
  const frame = {
    t: "WebhookEvent",
    webhook: webhookId,
    source,
    event,
    payload: parsed.payload ?? parsed,
    received_at: new Date().toISOString(),
  };
  const resp = await fetch(
    `${Deno.env.get("SUPABASE_URL")}/realtime/v1/api/broadcast`,
    {
      method: "POST",
      headers: {
        authorization: `Bearer ${SERVICE_ROLE}`,
        "content-type": "application/json",
        apikey: SERVICE_ROLE,
      },
      body: JSON.stringify({
        messages: [
          {
            topic: `machines:${hook.machine_id}`,
            event: "broadcast",
            payload: { event: "frame", payload: frame },
          },
        ],
      }),
    },
  );
  if (!resp.ok) {
    await log(502, source, event);
    return json(502, { error: "relay failed" });
  }

  await log(202, source, event);
  return json(202, { ok: true });
});
