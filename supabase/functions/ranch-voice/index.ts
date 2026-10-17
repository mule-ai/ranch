// Ranch voice gateway — Supabase Edge Function (Herd cloud-voice).
//
// Lets a phone that is OFF a user's local network use the user's voice
// stack through the public Supabase edge. The Supabase user JWT (Google
// or password login) is the credential: it identifies the user AND is
// the routing key — each user's requests are forwarded to the endpoints
// recorded in THEIR `voice_gateways` row (migration 0009), read under
// RLS with the same user JWT, so no user can reach another user's
// endpoints and the raw forge/mule keys never reach the phone.
//
//   Auth on every request:  Authorization: Bearer <supabase user JWT>
//   Routes (path mirroring, so client code is unchanged):
//     POST /v1/audio/transcriptions  -> {forge_base}/v1/audio/transcriptions
//     POST /v1/audio/speech          -> {forge_base}/v1/audio/speech
//     POST /api/v1/jobs              -> {mule_base}/api/v1/jobs
//     GET  /api/v1/workflows*        -> {mule_base}/api/v1/workflows*
//     GET  /gateway                  -> {mule_base_url: "…"} (public base
//                                       for the call's WebSocket, which
//                                       the Supabase edge cannot carry —
//                                       WS upgrades are rejected at the
//                                       platform level; the sockets go
//                                       phone → the user's own public
//                                       tunnel instead)
//
//   The mule voice sockets carry no API key by design: the 128-bit job
//   id IS the capability (mule server.go, H6.3). The public tunnel
//   allowlists /voice/* to that mule only; job ids are never logged.
//
// Env (all provided by the edge runtime — no secrets to set):
//   SUPABASE_URL, SUPABASE_ANON_KEY
//
// Caps: 10 MB audio body; 30 transcribe/speech req/min/user; 10 jobs/min.

const SUPABASE_URL = Deno.env.get("SUPABASE_URL")!;
const ANON = Deno.env.get("SUPABASE_ANON_KEY")!;

const BODY_CAP = 10 * 1024 * 1024;
const RATE_PER_MIN = 30; // per uid, audio routes
const JOBS_PER_MIN = 10;

// ------------------------------------------------------------------
// small helpers
// ------------------------------------------------------------------

function err(status: number, msg: string): Response {
  return new Response(JSON.stringify({ error: msg }), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** Verify the Supabase user JWT via GoTrue (works with the anon key).
 *  Returns the user id, or null when absent/rejected. 60 s token cache. */
const idCache = new Map<string, { uid: string; exp: number }>();
async function userId(req: Request): Promise<string | null> {
  const token = (req.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, "");
  if (!token) return null;
  const now = Date.now();
  const hit = idCache.get(token);
  if (hit && hit.exp > now) return hit.uid;
  try {
    const r = await fetch(`${SUPABASE_URL}/auth/v1/user`, {
      headers: { apikey: ANON, Authorization: `Bearer ${token}` },
    });
    if (!r.ok) return null;
    const user = await r.json();
    if (typeof user?.id !== "string") return null;
    idCache.set(token, { uid: user.id, exp: now + 60_000 });
    if (idCache.size > 200) idCache.delete(idCache.keys().next().value as string);
    return user.id;
  } catch {
    return null;
  }
}

/** Read the caller's OWN gateway row (RLS with the user JWT — the
 *  function never uses the service role for this table). */
async function gateway(req: Request, uid: string): Promise<{
  forgeBase: string; forgeKey: string; muleBase: string; muleKey: string;
} | null> {
  const token = (req.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, "");
  const r = await fetch(
    `${SUPABASE_URL}/rest/v1/voice_gateways?uid=eq.${uid}&select=forge_base_url,forge_api_key,mule_base_url,mule_api_key`,
    {
      headers: {
        apikey: ANON,
        Authorization: `Bearer ${token}`,
        Accept: "application/json",
      },
    },
  );
  if (!r.ok) return null;
  const rows = await r.json();
  const row = Array.isArray(rows) ? rows[0] : null;
  if (!row) return null;
  const norm = (u: unknown) =>
    typeof u === "string" ? u.trim().replace(/\/+$/, "") : "";
  return {
    forgeBase: norm(row.forge_base_url),
    forgeKey: String(row.forge_api_key ?? ""),
    muleBase: norm(row.mule_base_url),
    muleKey: String(row.mule_api_key ?? ""),
  };
}

// per-uid rate buckets (in-memory; good enough for v1 caps)
const rate = new Map<string, { n: number; t: number }>();
function rateOk(uid: string, cap: number): boolean {
  const now = Date.now();
  const b = rate.get(uid);
  if (!b || now - b.t > 60_000) {
    rate.set(uid, { n: 1, t: now });
    return true;
  }
  if (b.n >= cap) return false;
  b.n++;
  return true;
}

async function forwardHttp(
  req: Request,
  targetBase: string,
  path: string,
  keyHeader: [string, string],
): Promise<Response> {
  const len = Number(req.headers.get("content-length") ?? 0);
  if (len > BODY_CAP) return err(413, "body too large");
  const fwd = await fetch(`${targetBase}${path}`, {
    method: req.method,
    headers: {
      "content-type": req.headers.get("content-type") ?? "application/octet-stream",
      [keyHeader[0]]: keyHeader[1],
    },
    body: req.body,
    duplex: "half",
  });
  return new Response(fwd.body, {
    status: fwd.status,
    headers: {
      "content-type": fwd.headers.get("content-type") ?? "application/octet-stream",
      "cache-control": "no-store",
    },
  });
}

// ------------------------------------------------------------------
// router
// ------------------------------------------------------------------

Deno.serve(async (req: Request): Promise<Response> => {
  const url = new URL(req.url);
  // The runtime's req.url is the INTERNAL url: http://<host>/ranch-voice/<path>
  // (the public /functions/v1/ranch-voice prefix is stripped by the
  // platform) — so strip the function slug to mirror backend paths.
  let path = url.pathname.replace(/\/+$/, "");
  const slug = "/ranch-voice";
  if (path.startsWith(`${slug}/`)) path = path.slice(slug.length).replace(/\/+$/, "");
  else if (path === slug) path = "/";

  if (req.method === "GET" && path === "/health") {
    return new Response(JSON.stringify({ ok: true }), {
      headers: { "content-type": "application/json" },
    });
  }

  const uid = await userId(req);
  if (!uid) return err(401, "authenticate with your Supabase account");
  const gw = await gateway(req, uid);
  if (!gw) {
    return err(503, "no voice gateway configured (add your lab endpoints via the app settings or rpc/upsert_voice_gateway)");
  }

  // forge audio (STT/TTS) + voice-model picker (GET /v1/models —
  // forge profile names are the model ids the call screen offers)
  if (
    (req.method === "POST" &&
      (path === "/v1/audio/transcriptions" || path === "/v1/audio/speech")) ||
    (req.method === "GET" && path === "/v1/models")
  ) {
    if (req.method === "POST" && !rateOk(uid, RATE_PER_MIN)) return err(429, "rate limit");
    try {
      return await forwardHttp(req, gw.forgeBase, path, [
        "X-API-Key",
        gw.forgeKey,
      ]);
    } catch (e) {
      return err(502, `forge: ${String(e).slice(0, 120)}`);
    }
  }

  // mule: start the voice-call job; list workflows (call setup finds the
  // voice_call workflow by name)
  if (path === "/api/v1/jobs" && (req.method === "POST" || req.method === "GET")) {
    if (req.method === "POST" && !rateOk(uid, JOBS_PER_MIN)) return err(429, "rate limit");
    try {
      return await forwardHttp(req, gw.muleBase, path, [
        "Authorization",
        `Bearer ${gw.muleKey}`,
      ]);
    } catch (e) {
      return err(502, `mule: ${String(e).slice(0, 120)}`);
    }
  }
  // workflows + their steps (call setup finds the voice_call workflow by
  // matching config.agent_id over EVERY workflow's steps —
  // CallActivity.findVoiceWorkflow / the CLI's pick_voice_workflow both
  // GET /api/v1/workflows/{id}/steps, so the whole /api/v1/workflows*
  // prefix must forward, not just the collection — 2026-10-16: cloud calls
  // silently found "no voice_call workflow" because steps lookups 404'd)
  if (req.method === "GET" && path.startsWith("/api/v1/workflows")) {
    try {
      return await forwardHttp(req, gw.muleBase, path, [
        "Authorization",
        `Bearer ${gw.muleKey}`,
      ]);
    } catch (e) {
      return err(502, `mule: ${String(e).slice(0, 120)}`);
    }
  }

  // the public base the call's WebSockets connect to (Supabase edge
  // cannot carry WS upgrades — verified: the platform rejects them with
  // a bare 502 even for a plain JSON response)
  if (req.method === "GET" && path === "/gateway") {
    return new Response(JSON.stringify({ mule_base_url: gw.muleBase }), {
      headers: { "content-type": "application/json" },
    });
  }

  return err(404, "unknown route (this function proxies /v1/audio/*, GET /v1/models, /api/v1/jobs, /api/v1/workflows, GET /gateway)");
});
