#!/usr/bin/env node
// ranch relay test tool — raw Supabase Realtime client.
//
// Usage:
//   node tools/relay-test.mjs listen  <machine-email> <machine-key> [machine-id]
//   node tools/relay-test.mjs send    <machine-id> <frame-json>   (owner auth)
//   node tools/relay-test.mjs probe   <machine-id>                (join w/ owner, no traffic)
//
// Auth: owner credentials come from ~/.config/ranch/supabase-project.env
// (SUPABASE_REF, ANON_KEY, OWNER_EMAIL, OWNER_PASSWORD).

import fs from "node:fs";

const envFile = `${process.env.HOME}/.config/ranch/supabase-project.env`;
const env = Object.fromEntries(
  fs.readFileSync(envFile, "utf8")
    .split("\n")
    .filter((l) => l.startsWith("SUPABASE_") || l.startsWith("OWNER_") || l.startsWith("MACHINE_"))
    .map((l) => l.slice(0, l.indexOf("=")).trim())
    .map((k) => [k, null]),
);
// simpler: parse manually
const cfg = {};
for (const line of fs.readFileSync(envFile, "utf8").split("\n")) {
  const i = line.indexOf("=");
  if (i < 0) continue;
  let v = line.slice(i + 1).trim();
  if (v.startsWith('"') && v.endsWith('"')) v = v.slice(1, -1);
  cfg[line.slice(0, i).trim()] = v;
}

const URL = cfg.SUPABASE_URL;
const ANON = cfg.ANON_KEY;

async function login(email, password) {
  const r = await fetch(`${URL}/auth/v1/token?grant_type=password`, {
    method: "POST",
    headers: { apikey: ANON, "Content-Type": "application/json" },
    body: JSON.stringify({ email, password }),
  });
  const d = await r.json();
  if (!d.access_token) throw new Error(`login failed for ${email}: ${JSON.stringify(d)}`);
  return d.access_token;
}

let ref = 0;
const nextRef = () => String(++ref);

// WS URL always carries the ANON key as apikey; the user's JWT (if any)
// is sent in the phx_join payload as `access_token` (sibling of config).
// Private channel topic: `realtime:machines:<machine_id>` — Realtime gates
// it via RLS on the `machines` table (row id = machine id).
function connect() {
  const ws = new WebSocket(`${URL}/realtime/v1?apikey=${ANON}&vsn=1.0.0`);
  const handlers = { join: null, msg: null, err: null, open: null };
  ws.onopen = () => handlers.open?.(ws);
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.event === "phx_reply" && handlers.join && m.ref === api.pending) {
      const h = handlers.join;
      handlers.join = null;
      h(m.payload, m.ref);
    } else if (handlers.msg) {
      handlers.msg(m);
    }
  };
  ws.onerror = (e) => handlers.err?.(e);
  ws.onclose = (e) => console.error(`[ws] closed code=${e.code} reason=${e.reason}`);
  const api = {
    ws,
    send(obj) { ws.send(JSON.stringify(obj)); },
    join(topic, payload, onOk) {
      const r = nextRef();
      api.pending = r;
      handlers.join = (payload_, r_) => {
        if (payload_.status === "ok") onOk?.(payload_);
        else console.error(`[join] ${payload_.status}: ${JSON.stringify(payload_.response)}`);
      };
      api.send({ topic, event: "phx_join", payload, ref: r, join_ref: r });
    },
    onMessage(fn) { handlers.msg = fn; },
    onOpen(fn) { handlers.open = fn; },
    heartbeat(ref_) {
      api.send({ topic: "phoenix", event: "phx_heartbeat", payload: {}, ref: ref_ });
    },
  };
  return api;
}

const [cmd, ...args] = process.argv.slice(2);

if (cmd === "probe") {
  // join with a given JWT (from arg or owner) to see what the server says
  const machineId = args[0];
  const jwtArg = args[1]; // optional: "owner" | "machine" | "none" | raw jwt
  (async () => {
    let jwt = null;
    if (!jwtArg || jwtArg === "none") {
      // anon: expect RLS denial for the private channel
    } else if (jwtArg === "machine") {
      jwt = await login(cfg.MACHINE_EMAIL, cfg.MACHINE_KEY);
    } else if (jwtArg === "owner") {
      jwt = await login(cfg.OWNER_EMAIL, cfg.OWNER_PASSWORD);
    } else {
      jwt = jwtArg;
    }
    const topic = `realtime:machines:${machineId}`;
    const c = connect();
    c.onMessage((m) => {
      if (m.event === "phx_heartbeat") {
        console.log(`[hb] replying ref=${m.join_ref ?? m.ref}`);
        c.heartbeat(m.join_ref ?? m.ref);
      } else if (m.event === "broadcast") {
        console.log(`[broadcast] ${m.payload?.event}: ${JSON.stringify(m.payload?.payload).slice(0, 200)}`);
      } else {
        console.log(`[rt] ${m.topic} ${m.event}: ${JSON.stringify(m.payload).slice(0, 200)}`);
      }
    });
    c.onOpen(() => {
      console.log(`[probe] joining ${topic} (jwt kind: ${jwtArg ?? "none"})`);
      c.join(topic, {
        config: { broadcast: {}, presence: {}, postgres_changes: [], private: true },
        access_token: jwt,
      },
        (ok) => console.log(`[probe] join OK: ${JSON.stringify(ok).slice(0, 200)}`));
    });
    setTimeout(() => { console.log("[probe] closing"); process.exit(0); }, 15000);
  })().catch((e) => { console.error(e); process.exit(1); });
} else if (cmd === "listen") {
  // act as the "phone": join the machine channel, print frames, optionally echo back
  const machineEmail = args[0];
  const machineKey = args[1];
  const machineId = args[2];
  (async () => {
    const jwt = machineEmail ? await login(machineEmail, machineKey)
      : await login(cfg.OWNER_EMAIL, cfg.OWNER_PASSWORD);
    const topic = `realtime:machines:${machineId}`;
    let lastSeq = 0;
    let gapSeen = false;
    const c = connect();
    c.onMessage((m) => {
      if (m.event === "phx_heartbeat") { c.heartbeat(m.join_ref ?? m.ref); return; }
      if (m.event !== "broadcast" || m.payload?.event !== "frame") return;
      const f = m.payload.payload;
      const kind = f.t;
      const extra = kind === "Update" ? `seq=${f.seq} rows=${f.rows_upd?.length}`
        : kind === "Snapshot" ? `seq=${f.seq}` : JSON.stringify(f).slice(0, 120);
      console.log(`[frame] ${kind} ${extra}`);
      if (f.seq != null) {
        if (lastSeq && f.seq > lastSeq + 1) {
          gapSeen = true;
          console.log(`[gap] expected ${lastSeq + 1}, got ${f.seq} — resync needed`);
        }
        lastSeq = f.seq;
      }
    });
    c.onOpen(() => {
      console.log(`[listen] joining ${topic}`);
      c.join(topic, {
        config: { broadcast: {}, presence: {}, postgres_changes: [], private: true },
        access_token: jwt,
      },
        (ok) => console.log(`[listen] joined: ${JSON.stringify(ok).slice(0, 120)}`));
    });
    console.log(`[listen] up; Ctrl-C to stop (gapSeen will be printed on exit)`);
    process.on("SIGINT", () => {
      console.log(`[listen] stopping. seq-gap observed: ${gapSeen ? "YES (resync path exercised)" : "no"}`);
      process.exit(0);
    });
  })().catch((e) => { console.error(e); process.exit(1); });
} else if (cmd === "send") {
  // act as a remote client: broadcast a frame to the machine channel
  const machineId = args[0];
  const frame = JSON.parse(args[1]);
  const who = args[2]; // optional: "machine" (default owner)
  const jwt = who === "machine" ? await login(cfg.MACHINE_EMAIL, cfg.MACHINE_KEY)
    : await login(cfg.OWNER_EMAIL, cfg.OWNER_PASSWORD);
  const c = connect();
  const topic = `realtime:machines:${machineId}`;
  c.onOpen(() => {
    c.join(topic, {
      config: { broadcast: {}, presence: {}, postgres_changes: [], private: true },
      access_token: jwt,
    },
      () => {
        console.log(`[send] joined, broadcasting frame ${frame.t}`);
        c.send({
          topic,
          event: "broadcast",
          payload: { type: "broadcast", event: "frame", payload: frame },
          ref: nextRef(),
        });
        setTimeout(() => { console.log("[send] done"); process.exit(0); }, 2000);
      });
  });
} else {
  console.error("usage: relay-test.mjs <probe|listen|send> ...");
  process.exit(2);
}
