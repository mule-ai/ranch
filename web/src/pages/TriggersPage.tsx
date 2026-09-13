// Triggers page (Phase D/E): cron + event triggers and webhooks.
// One page, two sections — the pages share the relay and the "things
// that run workflows" mental model. Webhook editing (secrets/URLs)
// is create-once; the raw secret shows exactly once after creation.
import { useCallback, useEffect, useState } from "react";
import { Relay } from "../lib/relay";
import {
  Frame,
  WorkflowSummary,
  TriggerRow,
  WebhookRow,
  nextId,
} from "../lib/frames";

const EVENT_KINDS = [
  "agent_turn_ended",
  "workflow_completed",
  "file_changed",
  "pane_exited",
];

// human preview of a cron expression (covers the common shapes)
function cronPreview(expr: string): string {
  const m = expr.trim().split(/\s+/);
  if (m.length !== 5) return expr;
  const [mi, h] = m;
  if (mi.startsWith("*/") && h === "*") return `every ${mi.slice(2)} min`;
  if (h === "*") return `hourly at :${mi}`;
  if (mi === "0" && h.startsWith("*/")) return `every ${h.slice(2)} h at :00`;
  const days = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
  const dom = m[2] === "*" ? "" : ` day ${m[2]}`;
  const dow = m[4] !== "*" ? ` ${days[Number(m[4])] ?? m[4]}` : "";
  return `daily at ${h.padStart(2, "0")}:${mi.padStart(2, "0")}${dom}${dow}`;
}

type Props = {
  relay: Relay | null;
  onAttach: (sessionId: string) => void;
};

export function TriggersPage({ relay, onAttach }: Props) {
  const [triggers, setTriggers] = useState<TriggerRow[] | null>(null);
  const [webhooks, setWebhooks] = useState<WebhookRow[] | null>(null);
  const [workflows, setWorkflows] = useState<WorkflowSummary[]>([]);
  const [err, setErr] = useState("");
  const [editing, setEditing] = useState<TriggerRow | null>(null);
  const [newHookName, setNewHookName] = useState("");
  const [lastSecret, setLastSecret] = useState<{ url: string; secret: string } | null>(null);

  const refresh = useCallback(() => {
    if (!relay) return;
    relay.send({ t: "TriggerList", id: nextId(), req_id: nextId() } as unknown as Frame);
    relay.send({ t: "WebhookList", id: nextId(), req_id: nextId() } as Frame);
    relay.send({ t: "WorkflowList", id: nextId(), req_id: nextId() } as Frame);
  }, [relay]);

  useEffect(() => {
    if (!relay) return;
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "TriggerListOk":
          setTriggers(f.triggers as unknown as TriggerRow[]);
          break;
        case "WebhookListOk":
          setWebhooks(f.webhooks as unknown as WebhookRow[]);
          break;
        case "WorkflowListOk":
          setWorkflows(f.workflows);
          break;
        case "TriggerPutOk":
          setEditing(null);
          refresh();
          break;
        case "TriggerDeleteOk":
        case "WebhookDeleteOk":
          refresh();
          break;
        case "WebhookPutOk":
          setLastSecret({ url: f.url, secret: f.secret ?? "" });
          setNewHookName("");
          refresh();
          break;
        case "TriggerFired":
          // a trigger fired (possibly on another client) — refresh last_run
          refresh();
          break;
        case "Error":
          if (f.req_id) setErr(f.message);
          break;
      }
    });
    refresh();
    return un;
  }, [relay, refresh]);

  const wfName = (id: string) => workflows.find((w) => w.id === id)?.name ?? id.slice(0, 8);

  if (editing) {
    return (
      <TriggerForm
        relay={relay}
        workflows={workflows}
        initial={editing}
        onDone={() => setEditing(null)}
      />
    );
  }

  return (
    <div className="page narrow">
      <p className="rowline">
        <span className="title-inline">triggers</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}

      {lastSecret && (
        <div className="machrow" style={{ display: "block", borderColor: "#a855f7" }}>
          <b>webhook created — copy the secret now (shown once)</b>
          <pre className="dim" style={{ whiteSpace: "pre-wrap", fontSize: 12 }}>
            {`url: ${lastSecret.url}\nsecret: ${lastSecret.secret}`}
          </pre>
          <div className="btnrow">
            <button className="btn btn-ghost" onClick={() => setLastSecret(null)}>
              got it
            </button>
          </div>
        </div>
      )}

      <b>triggers</b>
      {triggers === null && <p className="dim">loading…</p>}
      {triggers !== null && triggers.length === 0 && (
        <p className="dim">No triggers. Triggers run workflows on a schedule or on events.</p>
      )}
      {triggers?.map((t) => (
        <div key={t.id} className="machrow" style={{ display: "block" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
            <b>{t.name}</b>
            <span className="dim" style={{ fontSize: 12 }}>
              {t.kind === "cron" && t.spec.cron ? cronPreview(t.spec.cron) : t.kind}
              {t.enabled ? "" : " · disabled"}
            </span>
          </div>
          <div className="dim" style={{ fontSize: 12 }}>
            runs {wfName(t.workflow_id)}
            {t.last_run && ` · last: ${new Date(t.last_run.at * 1000).toLocaleString()} (${t.last_run.status})`}
          </div>
          <div className="btnrow" style={{ marginTop: 6 }}>
            <button
              className="btn btn-ghost"
              onClick={() => relay?.send({ t: "TriggerRun", id: nextId(), req_id: nextId(), trigger: t.id! } as unknown as Frame)}
            >
              run now
            </button>
            <button
              className="btn btn-ghost"
              onClick={() =>
                relay?.send({
                  t: "TriggerPut", id: nextId(), req_id: nextId(),
                  trigger_id: t.id, trigger: { ...t, enabled: !t.enabled },
                } as unknown as Frame)
              }
            >
              {t.enabled ? "disable" : "enable"}
            </button>
            <button className="btn btn-ghost" onClick={() => setEditing(t)}>edit</button>
            <button
              className="btn btn-ghost danger"
              onClick={() => {
                if (!confirm(`delete trigger "${t.name}"?`)) return;
                relay?.send({ t: "TriggerDelete", id: nextId(), req_id: nextId(), trigger: t.id! } as unknown as Frame);
              }}
            >
              delete
            </button>
          </div>
        </div>
      ))}
      <div className="btnrow" style={{ marginTop: 12 }}>
        <button
          className="btn btn-primary"
          onClick={() =>
            setEditing({
              name: "",
              workflow_id: workflows[0]?.id ?? "",
              kind: "cron",
              spec: { cron: "0 3 * * *" },
              enabled: true,
            })
          }
          disabled={workflows.length === 0}
          title={workflows.length === 0 ? "create a workflow first" : undefined}
        >
          new trigger
        </button>
      </div>

      <b style={{ display: "block", marginTop: 24 }}>webhooks</b>
      {webhooks === null && <p className="dim">loading…</p>}
      {webhooks !== null && webhooks.length === 0 && (
        <p className="dim">No webhooks. A webhook is a URL an external service POSTs to start a workflow.</p>
      )}
      {webhooks?.map((w) => (
        <div key={w.id} className="machrow" style={{ display: "block" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
            <b>{w.name}</b>
            <span className="dim" style={{ fontSize: 12 }}>
              {w.sources?.length ? w.sources.join(", ") : "any source"}
            </span>
          </div>
          <div className="btnrow" style={{ marginTop: 6 }}>
            <button
              className="btn btn-ghost danger"
              onClick={() => {
                if (!confirm(`delete webhook "${w.name}"?`)) return;
                relay?.send({ t: "WebhookDelete", id: nextId(), req_id: nextId(), webhook: w.id } as Frame);
              }}
            >
              delete
            </button>
          </div>
        </div>
      ))}
      <div className="btnrow" style={{ marginTop: 12 }}>
        <input
          className="text-input"
          style={{ flex: 1 }}
          placeholder="new webhook name"
          value={newHookName}
          onChange={(e) => setNewHookName(e.target.value)}
        />
        <button
          className="btn btn-primary"
          disabled={!newHookName.trim()}
          onClick={() =>
            relay?.send({
              t: "WebhookPut", id: nextId(), req_id: nextId(),
              name: newHookName.trim(), sources: [],
            } as Frame)
          }
        >
          create
        </button>
      </div>
    </div>
  );
}

function TriggerForm({
  relay,
  workflows,
  initial,
  onDone,
}: {
  relay: Relay | null;
  workflows: WorkflowSummary[];
  initial: TriggerRow;
  onDone: () => void;
}) {
  const [t, setT] = useState<TriggerRow>(initial);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const set = (patch: Partial<TriggerRow>) => setT((x) => ({ ...x, ...patch }));
  const setSpec = (patch: Partial<TriggerRow["spec"]>) => setT((x) => ({ ...x, spec: { ...x.spec, ...patch } }));

  const save = () => {
    if (!relay) return;
    if (!t.name.trim() || !t.workflow_id) {
      setErr("name and workflow are required");
      return;
    }
    if (t.kind === "cron" && !/^\S+\s+\S+\s+\S+\s+\S+\s+\S+$/.test(t.spec.cron ?? "")) {
      setErr("cron must have 5 fields (m h dom mon dow)");
      return;
    }
    setBusy(true);
    const rid = nextId();
    relay.send({
      t: "TriggerPut", id: nextId(), req_id: rid,
      trigger_id: t.id ?? null, trigger: t,
    } as unknown as Frame);
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "TriggerPutOk" && f.req_id === rid) {
        un();
        onDone();
      } else if (f.t === "Error" && f.req_id === rid) {
        un();
        setErr(f.message);
        setBusy(false);
      }
    });
  };

  return (
    <div className="page narrow">
      <p className="rowline">
        <span className="title-inline">{t.id ? `edit: ${t.name}` : "new trigger"}</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}
      <label style={{ display: "block", marginBottom: 10 }}>
        <span className="dim" style={{ fontSize: 12 }}>name</span>
        <input className="text-input" style={{ width: "100%", display: "block" }} value={t.name} onChange={(e) => set({ name: e.target.value })} />
      </label>
      <label style={{ display: "block", marginBottom: 10 }}>
        <span className="dim" style={{ fontSize: 12 }}>workflow to run</span>
        <select style={{ display: "block", width: "100%" }} value={t.workflow_id} onChange={(e) => set({ workflow_id: e.target.value })}>
          {workflows.map((w) => (
            <option key={w.id} value={w.id}>{w.name}</option>
          ))}
        </select>
      </label>
      <label style={{ display: "block", marginBottom: 10 }}>
        <span className="dim" style={{ fontSize: 12 }}>kind</span>
        <select
          style={{ display: "block", width: "100%" }}
          value={t.kind}
          onChange={(e) => set({ kind: e.target.value as TriggerRow["kind"], spec: {} })}
        >
          <option value="cron">cron (schedule)</option>
          <option value="event">event (ranch events)</option>
          <option value="webhook">webhook (external POST)</option>
        </select>
      </label>
      {t.kind === "cron" && (
        <label style={{ display: "block", marginBottom: 10 }}>
          <span className="dim" style={{ fontSize: 12 }}>cron (5 fields, UTC)</span>
          <input className="text-input" style={{ width: "100%", display: "block", fontFamily: "monospace" }} value={t.spec.cron ?? ""} onChange={(e) => setSpec({ cron: e.target.value })} />
          {t.spec.cron && <span className="dim" style={{ fontSize: 12 }}>{cronPreview(t.spec.cron)}</span>}
        </label>
      )}
      {t.kind === "event" && (
        <>
          <label style={{ display: "block", marginBottom: 10 }}>
            <span className="dim" style={{ fontSize: 12 }}>event</span>
            <select
              style={{ display: "block", width: "100%" }}
              value={t.spec.event ?? EVENT_KINDS[0]}
              onChange={(e) => setSpec({ event: e.target.value })}
            >
              {EVENT_KINDS.map((k) => (
                <option key={k} value={k}>{k}</option>
              ))}
            </select>
          </label>
          {t.spec.event === "workflow_completed" && (
            <label style={{ display: "block", marginBottom: 10 }}>
              <span className="dim" style={{ fontSize: 12 }}>only when this workflow completes (optional)</span>
              <select
                style={{ display: "block", width: "100%" }}
                value={(t.spec.filter?.workflow_id as string) ?? ""}
                onChange={(e) => setSpec({ filter: e.target.value ? { workflow_id: e.target.value } : {} })}
              >
                <option value="">any workflow</option>
                {workflows.map((w) => (
                  <option key={w.id} value={w.id}>{w.name}</option>
                ))}
              </select>
            </label>
          )}
        </>
      )}
      {t.kind === "webhook" && (
        <p className="dim">Webhook triggers fire when an external service POSTs to a webhook URL. Create the webhook below the trigger list.</p>
      )}
      <label style={{ display: "block", marginBottom: 10 }}>
        <span className="dim" style={{ fontSize: 12 }}>
          <input type="checkbox" checked={t.catch_up ?? false} onChange={(e) => set({ catch_up: e.target.checked })} /> catch up on missed schedule (cron)
        </span>
      </label>
      <div className="btnrow" style={{ marginTop: 12 }}>
        <button className="btn btn-primary" onClick={save} disabled={busy}>
          {busy ? "saving…" : "save"}
        </button>
        <button className="btn btn-ghost" onClick={onDone} disabled={busy}>
          cancel
        </button>
      </div>
    </div>
  );
}
