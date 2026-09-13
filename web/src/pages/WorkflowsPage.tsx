// Workflows page (Phase C): browse/edit/run/delete mule workflows
// through the daemon's mule proxy. The editor is the reference
// implementation (step cards + config JSON); runs stream into a
// workflow pane the user attaches to from the session list.
import { useCallback, useEffect, useState } from "react";
import { Relay } from "../lib/relay";
import {
  Frame,
  WorkflowSummary,
  WorkflowStep,
  WorkflowDraft,
  nextId,
} from "../lib/frames";

type Props = {
  relay: Relay | null;
  onAttach: (sessionId: string) => void;
};

export function WorkflowsPage({ relay, onAttach }: Props) {
  const [workflows, setWorkflows] = useState<WorkflowSummary[] | null>(null);
  const [err, setErr] = useState("");
  const [editing, setEditing] = useState<{ id: string | null; draft: WorkflowDraft } | null>(null);
  const [runPane, setRunPane] = useState<{ session: string; workflow: string } | null>(null);

  const refresh = useCallback(() => {
    if (!relay) return;
    relay.send({ t: "WorkflowList", id: nextId(), req_id: nextId() } as Frame);
  }, [relay]);

  useEffect(() => {
    if (!relay) return;
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "WorkflowListOk":
          setWorkflows(f.workflows);
          break;
        case "WorkflowRunOk":
          setRunPane({ session: f.session, workflow: f.job });
          refresh();
          break;
        case "WorkflowPutOk":
          setEditing(null);
          refresh();
          break;
        case "WorkflowDeleteOk":
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

  if (editing) {
    return (
      <WorkflowForm relay={relay} workflowId={editing.id} initial={editing.draft} onDone={() => setEditing(null)} />
    );
  }

  return (
    <div className="page narrow">
      <p className="rowline">
        <span className="title-inline">workflows</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}
      {runPane && (
        <p className="rowline">
          <button className="btn btn-primary" onClick={() => onAttach(runPane.session)}>
            attach to the {runPane.workflow.slice(0, 8)} run pane →
          </button>
        </p>
      )}
      {workflows === null && <p className="dim">loading workflows…</p>}
      {workflows !== null && workflows.length === 0 && (
        <p className="dim">No mule workflows. Create one below.</p>
      )}
      {workflows?.map((w) => (
        <div key={w.id} className="machrow" style={{ display: "block" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
            <b>{w.name}</b>
            {w.is_async && <span className="dim" style={{ fontSize: 12 }}>async</span>}
          </div>
          {w.description && (
            <div className="dim" style={{ fontSize: 12 }}>{w.description}</div>
          )}
          <div className="btnrow" style={{ marginTop: 6 }}>
            <button
              className="btn btn-primary"
              onClick={() => relay?.send({ t: "WorkflowRun", id: nextId(), req_id: nextId(), workflow: w.id } as Frame)}
            >
              run
            </button>
            <button
              className="btn btn-ghost"
              onClick={async () => {
                if (!relay) return;
                const rid = nextId();
                relay.send({ t: "WorkflowGet", id: nextId(), req_id: rid, workflow: w.id } as Frame);
                const un = relay.onFrame((f: Frame) => {
                  if (f.t === "WorkflowGetOk" && f.req_id === rid) {
                    un();
                    setEditing({
                      id: f.workflow.id,
                      draft: {
                        name: f.workflow.name,
                        description: f.workflow.description,
                        is_async: f.workflow.is_async ?? false,
                        steps: f.steps,
                      },
                    });
                  } else if (f.t === "Error" && f.req_id === rid) {
                    un();
                    setErr(f.message);
                  }
                });
              }}
            >
              edit
            </button>
            <button
              className="btn btn-ghost danger"
              onClick={() => {
                if (!relay) return;
                if (!confirm(`delete workflow "${w.name}"?`)) return;
                relay.send({ t: "WorkflowDelete", id: nextId(), req_id: nextId(), workflow: w.id } as Frame);
              }}
            >
              delete
            </button>
          </div>
        </div>
      ))}
      <div className="btnrow" style={{ marginTop: 16 }}>
        <button
          className="btn btn-primary"
          onClick={() =>
            setEditing({
              id: null,
              draft: { name: "", description: "", is_async: false, steps: [] },
            })
          }
        >
          new workflow
        </button>
      </div>
    </div>
  );
}

function WorkflowForm({
  relay,
  workflowId,
  initial,
  onDone,
}: {
  relay: Relay | null;
  workflowId: string | null;
  initial: WorkflowDraft;
  onDone: () => void;
}) {
  const [draft, setDraft] = useState<WorkflowDraft>(initial);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const set = (patch: Partial<WorkflowDraft>) => setDraft((d) => ({ ...d, ...patch }));

  const setStep = (i: number, patch: Partial<WorkflowStep>) =>
    setDraft((d) => ({
      ...d,
      steps: d.steps.map((s, j) => (j === i ? { ...s, ...patch } : s)),
    }));

  const move = (i: number, delta: number) =>
    setDraft((d) => {
      const j = i + delta;
      if (j < 0 || j >= d.steps.length) return d;
      const steps = [...d.steps];
      [steps[i], steps[j]] = [steps[j], steps[i]];
      return { ...d, steps };
    });

  const save = () => {
    if (!relay) return;
    if (!draft.name.trim()) {
      setErr("name is required");
      return;
    }
    setBusy(true);
    const rid = nextId();
    relay.send({
      t: "WorkflowPut", id: nextId(), req_id: rid,
      workflow_id: workflowId,
      draft: { ...draft, steps: draft.steps.map((st, i) => ({ ...st, step_order: i })) },
    } as Frame);
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "WorkflowPutOk" && f.req_id === rid) {
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
        <span className="title-inline">{workflowId ? `edit: ${initial.name}` : "new workflow"}</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}
      <label style={{ display: "block", marginBottom: 10 }}>
        <span className="dim" style={{ fontSize: 12 }}>name</span>
        <input className="text-input" style={{ width: "100%", display: "block" }} value={draft.name} onChange={(e) => set({ name: e.target.value })} />
      </label>
      <label style={{ display: "block", marginBottom: 10 }}>
        <span className="dim" style={{ fontSize: 12 }}>description</span>
        <input className="text-input" style={{ width: "100%", display: "block" }} value={draft.description ?? ""} onChange={(e) => set({ description: e.target.value || null })} />
      </label>
      <label style={{ display: "block", marginBottom: 14 }}>
        <span className="dim" style={{ fontSize: 12 }}>
          <input type="checkbox" checked={draft.is_async} onChange={(e) => set({ is_async: e.target.checked })} /> async (fire and forget)
        </span>
      </label>

      <b>steps</b>
      {draft.steps.length === 0 && <p className="dim">No steps yet.</p>}
      {draft.steps.map((st, i) => (
        <div key={i} className="machrow" style={{ display: "block", margin: "8px 0" }}>
          <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
            <b>{i + 1}.</b>
            <select value={st.type} onChange={(e) => setStep(i, { type: e.target.value })}>
              <option value="agent">agent</option>
              <option value="wasm_module">wasm</option>
            </select>
            {st.type === "agent" && (
              <input
                className="text-input"
                style={{ flex: 1 }}
                placeholder="agent id"
                value={st.agent_id ?? ""}
                onChange={(e) => setStep(i, { agent_id: e.target.value || null })}
              />
            )}
            {st.type === "wasm_module" && (
              <input
                className="text-input"
                style={{ flex: 1 }}
                placeholder="wasm module id"
                value={st.wasm_module_id ?? ""}
                onChange={(e) => setStep(i, { wasm_module_id: e.target.value || null })}
              />
            )}
          </div>
          <textarea
            className="text-input"
            style={{ width: "100%", minHeight: 50, fontFamily: "monospace", fontSize: 12, marginTop: 6 }}
            placeholder="config (JSON)"
            value={JSON.stringify(st.config ?? {}, null, 0)}
            onChange={(e) => {
              try {
                const cfg = JSON.parse(e.target.value || "{}");
                setStep(i, { config: cfg });
              } catch {
                // keep typing; validation at save
              }
            }}
          />
          <div className="btnrow" style={{ marginTop: 4 }}>
            <button className="btn btn-ghost" onClick={() => move(i, -1)} disabled={i === 0}>↑</button>
            <button className="btn btn-ghost" onClick={() => move(i, 1)} disabled={i === draft.steps.length - 1}>↓</button>
            <button
              className="btn btn-ghost danger"
              onClick={() => setDraft((d) => ({ ...d, steps: d.steps.filter((_, j) => j !== i) }))}
            >
              remove step
            </button>
          </div>
        </div>
      ))}
      <div className="btnrow" style={{ marginTop: 8 }}>
        <button
          className="btn btn-ghost"
          onClick={() =>
            setDraft((d) => ({
              ...d,
              steps: [...d.steps, { id: null, step_order: d.steps.length, type: "agent", agent_id: "", wasm_module_id: null, config: {} }],
            }))
          }
        >
          add step
        </button>
      </div>

      <div className="btnrow" style={{ marginTop: 16 }}>
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
