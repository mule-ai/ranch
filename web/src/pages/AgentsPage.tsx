// Agents page (Phase B): browse/create/edit/delete forge agent profiles
// through the daemon's forge proxy. Mounted from MachineClient as a view
// toggle; talks to the same relay (frames: ProfileList/Get/Put/Delete).
import { useEffect, useState, useCallback } from "react";
import { Relay } from "../lib/relay";
import {
  Frame,
  ProfileSummary,
  ProfileDraft,
  nextId,
} from "../lib/frames";

// forge's provider allowlist (api/profiles.rs + migration 005 CHECK).
const PROVIDERS = [
  "openai",
  "anthropic",
  "proxy-anthropic",
  "proxy",
  "google",
  "gemini",
  "custom",
];

const KNOWN_TOOLS = ["bash", "read", "write", "edit"];

type Props = { relay: Relay | null; onLaunch: (profile: ProfileSummary) => void };

export function AgentsPage({ relay, onLaunch }: Props) {
  const [profiles, setProfiles] = useState<ProfileSummary[] | null>(null);
  const [err, setErr] = useState("");
  const [editing, setEditing] = useState<{ id: string | null; draft: ProfileDraft } | null>(null);

  const refresh = useCallback(() => {
    if (!relay) return;
    relay.send({ t: "ProfileList", id: nextId(), req_id: nextId() } as Frame);
  }, [relay]);

  useEffect(() => {
    if (!relay) return;
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "ProfileListOk":
          setProfiles(f.profiles);
          break;
        case "ProfilePutOk":
          setEditing(null);
          refresh();
          break;
        case "ProfileDeleteOk":
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
      <ProfileForm
        relay={relay}
        profileId={editing.id}
        initial={editing.draft}
        onCancel={() => setEditing(null)}
      />
    );
  }

  return (
    <div className="page narrow">
      <p className="rowline">
        <span className="title-inline">agents</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}
      {profiles === null && <p className="dim">loading profiles…</p>}
      {profiles !== null && profiles.length === 0 && (
        <p className="dim">No agent profiles yet. Create one below.</p>
      )}
      {profiles?.map((p) => (
        <div key={p.id} className="machrow" style={{ display: "block" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
            <b>{p.name}</b>
            <span className="dim" style={{ fontSize: 12 }}>
              {p.provider}/{p.model}
            </span>
          </div>
          {p.description && (
            <div className="dim" style={{ fontSize: 12 }}>
              {p.description}
            </div>
          )}
          <div className="btnrow" style={{ marginTop: 6 }}>
            <button
              className="btn btn-ghost"
              onClick={() => onLaunch(p)}
              title="open an agent pane running this profile"
            >
              launch
            </button>
            <button
              className="btn btn-ghost"
              onClick={async () => {
                if (!relay) return;
                const rid = nextId();
                relay.send({ t: "ProfileGet", id: nextId(), req_id: rid, profile: p.id } as Frame);
                // the reply arrives on the shared frame listener above;
                // intercept it here with a one-shot listener
                const un = relay.onFrame((f: Frame) => {
                  if (f.t === "ProfileGetOk" && f.req_id === rid) {
                    un();
                    setEditing({
                      id: f.profile.id,
                      draft: {
                        name: f.profile.name,
                        description: f.profile.description,
                        provider: f.profile.provider,
                        model: f.profile.model,
                        base_url: f.profile.base_url,
                        working_dir: f.profile.working_dir,
                        git_url: f.profile.git_url,
                        git_ref: f.profile.git_ref,
                        nix_shell: f.profile.nix_shell,
                        system_prompt: f.profile.system_prompt,
                        tools: f.profile.tools,
                        // api_key left unset: blank = keep existing
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
                if (!confirm(`delete profile "${p.name}"?`)) return;
                relay.send({ t: "ProfileDelete", id: nextId(), req_id: nextId(), profile: p.id } as Frame);
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
              draft: {
                name: "",
                provider: PROVIDERS[0],
                model: "",
                system_prompt: "",
                tools: ["bash", "read", "write", "edit"],
              },
            })
          }
        >
          new agent profile
        </button>
      </div>
    </div>
  );
}

function ProfileForm({
  relay,
  profileId,
  initial,
  onCancel,
}: {
  relay: Relay | null;
  profileId: string | null;
  initial: ProfileDraft;
  onCancel: () => void;
}) {
  const [draft, setDraft] = useState<ProfileDraft>(initial);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const set = (patch: Partial<ProfileDraft>) => setDraft((d) => ({ ...d, ...patch }));

  const save = () => {
    if (!relay) return;
    if (!draft.name.trim() || !draft.provider || !draft.model.trim()) {
      setErr("name, provider and model are required");
      return;
    }
    setBusy(true);
    const rid = nextId();
    relay.send({
      t: "ProfilePut",
      id: nextId(),
      req_id: rid,
      profile_id: profileId,
      draft: {
        ...draft,
        // blank api_key = keep the stored one (forge keeps the old value
        // when the field is absent on PATCH)
        api_key: draft.api_key?.trim() ? draft.api_key : profileId ? null : draft.api_key,
      },
    } as Frame);
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "ProfilePutOk" && f.req_id === rid) {
        un();
        onCancel(); // back to the list (which refreshes)
      } else if (f.t === "Error" && f.req_id === rid) {
        un();
        setErr(f.message);
        setBusy(false);
      }
    });
  };

  const field = (label: string, node: React.ReactNode) => (
    <label style={{ display: "block", marginBottom: 10 }}>
      <span className="dim" style={{ fontSize: 12 }}>{label}</span>
      {node}
    </label>
  );

  const input = (value: string, onChange: (v: string) => void, opts?: { placeholder?: string; type?: string }) => (
    <input
      className="text-input"
      style={{ width: "100%", display: "block" }}
      value={value}
      type={opts?.type ?? "text"}
      placeholder={opts?.placeholder}
      onChange={(e) => onChange(e.target.value)}
    />
  );

  return (
    <div className="page narrow">
      <p className="rowline">
        <span className="title-inline">{profileId ? `edit: ${initial.name}` : "new agent profile"}</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}
      {field("name", input(draft.name, (v) => set({ name: v })))}
      {field(
        "description",
        input(draft.description ?? "", (v) => set({ description: v || null })),
      )}
      <div style={{ display: "flex", gap: 12 }}>
        {field(
          "provider",
          <select
            value={draft.provider}
            onChange={(e) => set({ provider: e.target.value })}
            style={{ display: "block", width: "100%" }}
          >
            {PROVIDERS.map((p) => (
              <option key={p} value={p}>{p}</option>
            ))}
          </select>,
        )}
        {field("model", input(draft.model, (v) => set({ model: v }), { placeholder: "model id" }))}
      </div>
      {field(
        "system prompt",
        <textarea
          className="text-input"
          style={{ width: "100%", minHeight: 90, display: "block", fontFamily: "inherit" }}
          value={draft.system_prompt ?? ""}
          onChange={(e) => set({ system_prompt: e.target.value })}
        />,
      )}
      {field(
        "api key (write-only — blank keeps the stored key)",
        input(draft.api_key ?? "", (v) => set({ api_key: v }), { type: "password", placeholder: profileId ? "••••••" : "provider api key" }),
      )}
      {field(
        "working dir",
        input(draft.working_dir ?? "", (v) => set({ working_dir: v || null }), { placeholder: "absolute path" }),
      )}
      <div style={{ display: "flex", gap: 12 }}>
        {field("git url (optional)", input(draft.git_url ?? "", (v) => set({ git_url: v || null })))}
        {field("git ref (optional)", input(draft.git_ref ?? "", (v) => set({ git_ref: v || null })))}
      </div>
      {field(
        "tools",
        <div className="btnrow">
          {KNOWN_TOOLS.map((t) => (
            <label key={t} style={{ display: "inline-flex", gap: 4, alignItems: "center", marginRight: 12 }}>
              <input
                type="checkbox"
                checked={draft.tools.includes(t)}
                onChange={(e) =>
                  set({
                    tools: e.target.checked
                      ? [...draft.tools, t]
                      : draft.tools.filter((x) => x !== t),
                  })
                }
              />
              {t}
            </label>
          ))}
        </div>,
      )}
      <div className="btnrow" style={{ marginTop: 12 }}>
        <button className="btn btn-primary" onClick={save} disabled={busy}>
          {busy ? "saving…" : "save"}
        </button>
        <button className="btn btn-ghost" onClick={onCancel} disabled={busy}>
          cancel
        </button>
      </div>
    </div>
  );
}
