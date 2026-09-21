// The live client: login → machine picker → sessions (full management:
// create shell/agent/pi, rename, kill, dir picker, forge resume, hot
// upgrade) → terminal (split panes, keys, scrollback, agent chat).
import { useEffect, useMemo, useRef, useState, useCallback } from "react";
import { Relay } from "../lib/relay";
import {
  AgentAskRequest,
  ChatMsg,
  Frame,
  Layout,
  PaneSnap,
  SessionMeta,
  b64,
  nextId,
} from "../lib/frames";
import { PaneView, Rect, FS, LH } from "../components/PaneView";
import { supabase } from "../lib/supabase";
import { demoAvailable } from "../lib/demo";
import { checkUpdate, type UpdateInfo } from "../lib/version";
import { Login } from "./Login";
import { AgentsPage } from "./AgentsPage";
import { WorkflowsPage } from "./WorkflowsPage";
import { TriggersPage } from "./TriggersPage";
import type { ProfileSummary } from "../lib/frames";
import { clearSessionChat, getCachedChat, mergeChat, saveCachedChat } from "../lib/chatCache";

type Machine = { id: string; name: string; last_seen_at: string | null };

// how many chat rows to load on attach; older rows page in on scrollback
const CHAT_TAIL = 25;
const CHAT_PAGE = 50;

// /home/user/src/lab -> ~/src/lab ; otherwise keep the path as-is
const shortPath = (p?: string | null) =>
  p ? p.replace(/^\/home\/[^/]+(\/|$)/, "~$1") : undefined;

// HH:MM from a UTC ISO timestamp ("YYYY-MM-DDTHH:MM:SSZ"); null if absent.
function tsOf(s?: string) {
  return s && s.length >= 16 ? s.slice(11, 16) : null;
}

function layoutRects(l: Layout, x: number, y: number, w: number, h: number): Rect[] {
  if (l.k === "Leaf") return [{ pane: l.pane, x, y, w, h }];
  const pct = Math.min(Math.max(l.pct, 0), 100) / 100;
  if (l.dir === 1) {
    const aw = Math.max(1, Math.round(w * pct));
    return [
      ...layoutRects(l.a, x, y, aw, h),
      ...layoutRects(l.b, x + aw, y, w - aw, h),
    ];
  }
  const ah = Math.max(1, Math.round(h * pct));
  return [
    ...layoutRects(l.a, x, y, w, ah),
    ...layoutRects(l.b, x, y + ah, w, h - ah),
  ];
}

export function WebApp() {
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [machine, setMachine] = useState<Machine | null>(null);
  // update banner (stale client vs the published release)
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  useEffect(() => {
    checkUpdate().then(setUpdate).catch(() => {});
  }, []);

  useEffect(() => {
    supabase.auth.getSession().then(({ data }) => setAuthed(!!data.session));
    const { data: sub } = supabase.auth.onAuthStateChange((event, session) => {
      if (event === "SIGNED_OUT") setAuthed(false);
      else if (session) setAuthed(true);
    });
    return () => sub.subscription.unsubscribe();
  }, []);

  if (authed === null)
    return <p className="dim center">…</p>;
  if (!authed) return <Login onSignedIn={() => setAuthed(true)} />;
  if (!machine)
    return (
      <>
        <MachinePicker onPick={setMachine} onSignOut={() => supabase.auth.signOut()} />
        {update && <UpdateBanner update={update} />}
      </>
    );
  return <MachineClient machine={machine} onBack={() => setMachine(null)} />;
}

function UpdateBanner({ update, daemon }: { update: UpdateInfo; daemon?: string | null }) {
  return (
    <p className="err" style={{ background: "rgba(245,158,11,0.12)", color: "#f59e0b", margin: "8px 16px" }}>
      update available: this build is older than {update.latest} —{" "}
      <a href={update.apkUrl} style={{ color: "#f59e0b" }}>get the latest</a>
      {daemon ? <> · daemon runs {daemon} (restart ranchd after updating)</> : null}
    </p>
  );
}

function MachinePicker({
  onPick,
  onSignOut,
}: {
  onPick: (m: Machine) => void;
  onSignOut: () => void;
}) {
  const [machines, setMachines] = useState<Machine[] | null>(null);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    setErr("");
    const { data, error } = await supabase
      .from("machines_info")
      .select("id,name,last_seen_at")
      .order("name");
    if (error) setErr(error.message);
    setMachines(data ?? []);
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  const online = (m: Machine) =>
    m.last_seen_at !== null && Date.now() - Date.parse(m.last_seen_at) < 90_000;

  return (
    <div className="page narrow">
      <h1>Your machines</h1>
      {err !== "" && <p className="err">{err}</p>}
      {machines === null && <p className="dim">loading…</p>}
      {machines?.length === 0 && (
        <p className="dim">
          No machines registered. Run <code>ranch register</code> on a host.
        </p>
      )}
      {machines?.map((m) => (
        <button key={m.id} className="machrow" onClick={() => onPick(m)}>
          <span className="dot" style={{ background: online(m) ? "#4ade80" : "#6b7280" }} />
          <span className="machname">{m.name}</span>
          <span className="dim">
            {online(m)
              ? "online"
              : `last seen ${m.last_seen_at?.slice(0, 16).replace("T", " ") ?? "never"}`}
          </span>
        </button>
      ))}
      <div className="btnrow">
        <button className="btn btn-ghost" onClick={load}>Refresh</button>
        <button className="btn btn-ghost danger" onClick={onSignOut}>Sign out</button>
      </div>
    </div>
  );
}

function MachineClient({ machine, onBack }: { machine: Machine; onBack: () => void }) {
  const [relay, setRelay] = useState<Relay | null>(null);
  // update banner (stale client vs the published release)
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [daemonVersion, setDaemonVersion] = useState<string | null>(null);
  const [monitorPi, setMonitorPi] = useState<boolean>(false);
  useEffect(() => {
    checkUpdate().then(setUpdate).catch(() => {});
  }, []);
  const [sessions, setSessions] = useState<SessionMeta[] | null>(null);
  // null = sessions view; "agents" / "workflows" = builder pages
  const [view, setView] = useState<null | "agents" | "workflows" | "triggers">(null);
  const [pendingProfile, setPendingProfile] = useState<ProfileSummary | null>(null);
  const [attached, setAttached] = useState<SessionMeta | null>(null);
  const [conn, setConn] = useState("connecting…");
  const [err, setErr] = useState("");
  const [upgrading, setUpgrading] = useState(false);
  const upgradingRef = useRef(false);
  upgradingRef.current = upgrading;

  useEffect(() => {
    let r: Relay | null = null;
    let retryTimer: ReturnType<typeof setInterval> | null = null;
    let unlisten: (() => void) | null = null;
    let watchdog: ReturnType<typeof setInterval> | null = null;
    (async () => {
      setSessions(null);
      setErr("");
      r = new Relay(machine.id);
      let gotHello = false;
      const hello = () => r?.send({ t: "Hello", id: nextId(), client: "web" } as Frame);
      r.onReady = () => {
        gotHello = false;
        hello();
        if (retryTimer) clearInterval(retryTimer);
        retryTimer = setInterval(() => {
          if (!gotHello) hello();
        }, 3000);
      };
      watchdog = setInterval(() => {
        // 15s without ANY inbound frame/status → suspect a dead socket.
        // ensureAlive(30000) compares against its own default window;
        // the call only actually reconnects when the connection has
        // truly been quiet past the threshold. (A previous version
        // called ensureAlive(0), which reconnects unconditionally —
        // every 5s forever whenever no broadcast happened to arrive,
        // which looked like the page spazzing out on mobile.)
        if (r && Date.now() - r.lastInboundAt() > 45000) {
          r.ensureAlive(0);
        }
      }, 5000);
      unlisten = r.onFrame((f: Frame) => {
        r!.touch();
        switch (f.t) {
          case "HelloOk":
            gotHello = true;
            if (retryTimer) { clearInterval(retryTimer); retryTimer = null; }
            setSessions(f.sessions);
            setConn("online");
            if (f.version) setDaemonVersion(f.version);
            setMonitorPi(!!f.monitor_external_pi);
            // hot upgrade: daemon is back with the new binary
            setUpgrading(false);
            break;
          case "SessionsAck":
            // a session was created → re-pull the list so it shows up
            hello();
            break;
          case "Meta":
            // broadcast when a session is killed from elsewhere
            // (dashboard/CLI/another client) — refresh the list
            if (f.kind === "exited") hello();
            break;
          case "Error":
            setErr(f.message);
            break;
        }
      });
      r.onStatus = setConn;
      setRelay(r);
      try {
        await r.join();
      } catch (e: any) {
        setSessions([]);
        setErr(e.message ?? "realtime connection failed");
      }
    })();
    return () => {
      unlisten?.();
      if (retryTimer) clearInterval(retryTimer);
      if (watchdog) clearInterval(watchdog);
      r?.leave();
      setRelay(null);
      setSessions(null);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [machine.id]);

  const hotUpgrade = () => {
    setUpgrading(true);
    relay?.send({ t: "Upgrade" } as Frame);
    setTimeout(() => {
      if (upgradingRef.current) {
        setUpgrading(false);
        setErr("daemon did not come back after upgrade — refresh the page");
      }
    }, 30000);
  };

  if (attached && relay)
    return (
      <Terminal
        relay={relay}
        session={attached}
        onExit={() => {
          setAttached(null);
          relay.send({ t: "Hello", id: nextId(), client: "web" } as Frame);
        }}
        onKilled={() => {
          // session ended under us — back to the list (the ⋯-menu kill
          // path filters it there; this is the in-session kill button)
          setAttached(null);
          relay.send({ t: "Hello", id: nextId(), client: "web" } as Frame);
        }}
      />
    );

  return (
    <div className="page narrow">
      <p className="rowline">
        {view === "agents" ? (
          <button className="linkbtn" onClick={() => setView(null)}>‹ sessions</button>
        ) : (
          <button className="linkbtn" onClick={onBack}>‹ machines</button>
        )}
        <span className="title-inline">{machine.name}</span>
        {daemonVersion ? (
          <span className="dim" style={{ marginLeft: 8, fontFamily: "var(--mono)", fontSize: "0.8rem" }}>
            daemon {daemonVersion}
          </span>
        ) : null}
        <span className="conn-badge">{conn}</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}
      {update && <UpdateBanner update={update} daemon={daemonVersion} />}

      {view === "agents" && (
        <AgentsPage
          relay={relay}
          onLaunch={(profile) => {
            setPendingProfile(profile);
            setView(null); // back to sessions; CreateRow handles the launch
          }}
        />
      )}
      {view === "workflows" && (
        <WorkflowsPage
          relay={relay}
          onAttach={(sid) => {
            const s = sessions?.find((x) => x.id === sid);
            if (s) setAttached(s);
            setView(null);
          }}
        />
      )}
      {view === "triggers" && (
        <TriggersPage
          relay={relay}
          onAttach={(sid) => {
            const s = sessions?.find((x) => x.id === sid);
            if (s) setAttached(s);
            setView(null);
          }}
        />
      )}
      {view === null && (
      <>
      {sessions === null && <p className="dim">loading sessions…</p>}
      {sessions !== null && sessions.length === 0 && (
        <p className="dim">No sessions. Create one below.</p>
      )}
      {sessions?.map((s) => (
        <SessionRow
          key={s.id}
          session={s}
          relay={relay}
          onAttach={() => setAttached(s)}
          onKilled={() => setSessions((prev) => (prev ?? []).filter((x) => x.id !== s.id))}
          onRenamed={(name) =>
            setSessions((prev) => (prev ?? []).map((x) => (x.id === s.id ? { ...x, name } : x)))
          }
        />
      ))}

      <CreateRow relay={relay} pendingProfile={pendingProfile} onProfileLaunched={() => setPendingProfile(null)} monitorPi={monitorPi} setMonitorPi={setMonitorPi} />
      </>
      )}
      {view === null && (
      <div className="btnrow" style={{ marginTop: 24 }}>
        {/* demo build: hot upgrade is denied server-side for relay clients,
            so don't offer the button to public demo visitors */}
        {!demoAvailable && (
          <button className="btn btn-ghost" onClick={hotUpgrade} disabled={upgrading || !relay}>
            {upgrading ? "upgrading…" : "upgrade daemon"}
          </button>
        )}
        <button className="btn btn-ghost danger" onClick={() => supabase.auth.signOut()}>
          Sign out
        </button>
      </div>
      )}
      <div className="btnrow" style={{ marginTop: 12 }}>
        <button className="btn btn-ghost" onClick={() => setView(view === "agents" ? null : "agents")}>
          {view === "agents" ? "sessions" : "agents"}
        </button>
        <button className="btn btn-ghost" onClick={() => setView(view === "workflows" ? null : "workflows")}>
          {view === "workflows" ? "sessions" : "workflows"}
        </button>
        <button className="btn btn-ghost" onClick={() => setView(view === "triggers" ? null : "triggers")}>
          {view === "triggers" ? "sessions" : "triggers"}
        </button>
      </div>
    </div>
  );
}

function SessionRow({
  session,
  relay,
  onAttach,
  onKilled,
  onRenamed,
}: {
  session: SessionMeta;
  relay: Relay | null;
  onAttach: () => void;
  onKilled: () => void;
  onRenamed: (name: string) => void;
}) {
  const [menu, setMenu] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [text, setText] = useState(session.name);

  return (
    <div className="sessionrow">
      {renaming ? (
        <form
          className="renameform"
          onSubmit={(e) => {
            e.preventDefault();
            const name = text.trim() || session.name;
            relay?.send({ t: "SessionsRename", session: session.id, name } as Frame);
            onRenamed(name);
            setRenaming(false);
          }}
        >
          <input autoFocus value={text} onChange={(e) => setText(e.target.value)} onBlur={() => setRenaming(false)} />
          <button type="submit" className="btn btn-ghost">rename</button>
        </form>
      ) : (
        <>
          <button className="sessionmain" onClick={onAttach}>
            <span className="machname">{session.name}</span>
            <span className="dim">
              {session.kind} · {session.panes.length} pane{session.panes.length === 1 ? "" : "s"}
            </span>
          </button>
          <button className="iconbtn" title="session menu" onClick={() => setMenu(!menu)}>
            ⋯
          </button>
        </>
      )}
      {menu && (
        <div className="menu">
          <button onClick={() => { setRenaming(true); setText(session.name); setMenu(false); }}>
            rename
          </button>
          <button
            className="danger"
            onClick={() => {
              clearSessionChat(session.id);
              relay?.send({ t: "SessionsKill", session: session.id } as Frame);
              onKilled();
              setMenu(false);
            }}
          >
            kill session
          </button>
          <button onClick={() => setMenu(false)}>cancel</button>
        </div>
      )}
    </div>
  );
}

function CreateRow({ relay, pendingProfile, onProfileLaunched, monitorPi, setMonitorPi }: { relay: Relay | null; pendingProfile?: ProfileSummary | null; onProfileLaunched?: () => void; monitorPi: boolean; setMonitorPi: (v: boolean) => void }) {
  const [name, setName] = useState("");
  // Demo build: the agent is forge-backed with no tools (the demo API
  // key is restricted server-side — profile CRUD, working_dir anchors,
  // and tool execution are all denied). No local pi panes on the demo:
  // the public shouldn't reach any code path but the sandboxed forge
  // session tree.
  const kinds = demoAvailable
    ? (["shell", "forge"] as const)
    : (["shell", "forge", "pi"] as const);
  const [kind, setKind] = useState<(typeof kinds)[number]>("shell");
  const [piDir, setPiDir] = useState<string | null>(null);
  const [dirBrowse, setDirBrowse] = useState<{ path: string; parent: string | null; dirs: string[] } | null>(null);
  const [resumeList, setResumeList] = useState<{ kind: "forge" | "pi"; id: string; title: string; session_file?: string; updated?: string; ended?: string | null; active?: boolean; external?: boolean; path?: string; working_dir?: string | null }[] | null>(null);
  const [resumeQuery, setResumeQuery] = useState("");
  const [resumeFolder, setResumeFolder] = useState<string | null>(null);
  const dirReqRef = useRef<string | null>(null);
  const resumeReqRef = useRef<string | null>(null);
  const piResumeReqRef = useRef<string | null>(null);

  // frame listener for DirListOk / ForgeListOk
  useEffect(() => {
    if (!relay) return;
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "DirListOk" && f.req_id === dirReqRef.current) {
        dirReqRef.current = null;
        setDirBrowse({ path: f.path, parent: f.parent ?? null, dirs: f.dirs });
      } else if (f.t === "ForgeListOk" && f.req_id === resumeReqRef.current) {
        resumeReqRef.current = null;
        setResumeList((prev) => [...(prev ?? []), ...f.sessions.map((s) => ({ kind: "forge" as const, ...s }))]);
      } else if (f.t === "PiListOk" && f.req_id === piResumeReqRef.current) {
        piResumeReqRef.current = null;
        setResumeList((prev) => [...(prev ?? []), ...f.sessions.map((s) => ({ kind: "pi" as const, id: s.id, title: s.title, session_file: s.session_file, active: s.active, external: s.external, path: s.path, updated: s.updated }))]);
      } else if (f.t === "PiMonitorOk") {
        setMonitorPi(f.enabled);
      }
    });
    return un;
  }, [relay]);

  const browseDir = (path?: string) => {
    if (!relay) return;
    const rid = nextId();
    dirReqRef.current = rid;
    relay.send({ t: "DirList", id: nextId(), client: "web", req_id: rid, path } as Frame);
  };

  const create = () => {
    relay?.send({
      t: "SessionsCreate", req_id: nextId(), name: name || undefined,
      kind,
      cwd: kind === "pi" ? (piDir ?? undefined) : undefined,
    } as Frame);
  };

  // agent-builder launch: create a forge session bound to the picked profile
  useEffect(() => {
    if (!relay || !pendingProfile) return;
    relay.send({
      t: "SessionsCreate", req_id: nextId(), name: pendingProfile.name,
      kind: "forge", profile_id: pendingProfile.id,
    } as unknown as Frame);
    onProfileLaunched?.();
  }, [relay, pendingProfile, onProfileLaunched]);

  return (
    <div className="createrow">
      <div className="kindrow">
        {(kinds as readonly string[]).map((k) => (
          <button key={k} className={"chip" + (kind === k ? " chip-on" : "")} onClick={() => setKind(k as (typeof kinds)[number])}>
            {k === "forge" ? "agent (forge)" : k === "pi" ? "pi" : k}
          </button>
        ))}
        {!demoAvailable && (
          <button
            className="chip"
            onClick={() => {
              if (!relay) return;
              const rid = nextId();
              const prid = nextId();
              setResumeList([]);
              setResumeQuery("");
              setResumeFolder(null);
              resumeReqRef.current = rid;
              piResumeReqRef.current = prid;
              relay.send({ t: "ForgeList", id: nextId(), client: "web", req_id: rid } as Frame);
              relay.send({ t: "PiList", id: nextId(), client: "web", req_id: prid } as Frame);
            }}
          >
            resume…
          </button>
        )}
        {!demoAvailable && monitorPi !== null && (
          <button
            className={"chip" + (monitorPi ? " chip-on" : "")}
            onClick={() => {
              if (!relay) return;
              const enabled = !monitorPi;
              relay.send({ t: "PiMonitor", enabled, req_id: nextId() } as Frame);
              setMonitorPi(enabled);
            }}
          >
            pi-watch: {monitorPi ? "on" : "off"}
          </button>
        )}
      </div>

      {kind === "pi" && (
        <button className="chip chip-wide" onClick={() => browseDir(piDir ?? undefined)}>
          dir: {piDir ?? "$HOME"}
        </button>
      )}

      {dirBrowse !== null && (
        <div className="sheet">
          <b className="dim" style={{ fontSize: "0.8rem" }}>{dirBrowse.path}</b>
          {dirBrowse.dirs.map((d) => (
            <button key={d} className="machrow" onClick={() => browseDir(dirBrowse.path + "/" + d)}>
              <span className="machname">{d}/</span>
            </button>
          ))}
          {dirBrowse.dirs.length === 0 && <p className="dim">no subdirectories</p>}
          <div className="kindrow">
            {dirBrowse.parent !== null && (
              <button className="chip" onClick={() => browseDir(dirBrowse.parent ?? undefined)}>up…</button>
            )}
            <button className="chip chip-on" onClick={() => { setPiDir(dirBrowse.path); setDirBrowse(null); }}>
              use this dir
            </button>
            <button className="chip" onClick={() => setDirBrowse(null)}>cancel</button>
          </div>
        </div>
      )}

      {resumeList !== null && (() => {
        const folderOf = (i: { kind: string; path?: string; working_dir?: string | null }) =>
          i.kind === "forge" ? (i.working_dir ?? undefined) : (i.path ?? undefined);
        const folders = Array.from(new Set(resumeList.map(folderOf).filter((f): f is string => !!f))).sort();
        const q = resumeQuery.trim().toLowerCase();
        const filtered = resumeList.filter((i) => {
          const f = folderOf(i);
          if (resumeFolder !== null && f !== resumeFolder) return false;
          if (q && !`${i.title}\n${i.id}\n${f ?? ""}`.toLowerCase().includes(q)) return false;
          return true;
        });
        return (
        <div className="sheet">
          <div className="sheet-header">
            <b>sessions to resume</b>
            <button className="sheet-close" aria-label="close" onClick={() => { setResumeList(null); setResumeQuery(""); setResumeFolder(null); }}>✕</button>
          </div>
          <input
            className="resume-search"
            placeholder="search title, path…"
            value={resumeQuery}
            onChange={(e) => setResumeQuery(e.target.value)}
          />
          <p className="dim" style={{ marginBottom: 4 }}>
            showing {filtered.length} of {resumeList.length} session{resumeList.length === 1 ? "" : "s"}
          </p>
          {folders.length > 1 && (
            <div className="resume-chips">
              <button
                className={resumeFolder === null ? "chip chip-on" : "chip"}
                onClick={() => setResumeFolder(null)}
              >
                all ({resumeList.length})
              </button>
              {folders.map((f) => {
                const on = resumeFolder === f;
                const n = resumeList.filter((i) => folderOf(i) === f).length;
                return (
                  <button
                    key={f}
                    className={on ? "chip chip-on" : "chip"}
                    onClick={() => setResumeFolder(on ? null : f)}
                  >
                    {shortPath(f)} · {n}
                  </button>
                );
              })}
            </div>
          )}
          {filtered.map((item) => (
            <button
              key={item.id}
              className="machrow"
              onClick={() => {
                setResumeList(null);
                if (item.kind === "forge") {
                  relay?.send({
                    t: "SessionsCreate", req_id: nextId(), kind: "forge",
                    forge_session: item.id,
                  } as Frame);
                } else {
                  relay?.send({
                    t: "SessionsCreate", req_id: nextId(), kind: "pi",
                    pi_session_file: item.session_file,
                  } as Frame);
                }
              }}
            >
              <span className="machname">[{item.kind}{item.external ? "·ext" : ""}] {item.title || item.id.slice(0, 8)}</span>
              <span className="dim">
                {item.kind === "forge"
                  ? [shortPath(item.working_dir ?? undefined), item.ended ? "ended" : "active", item.updated?.slice(0, 16).replace("T", " ")].filter(Boolean).join(" · ")
                  : [shortPath(item.path ?? undefined), item.active ? "running" : "idle", item.updated ? new Date(parseInt(item.updated,10)*1000).toLocaleString("en-US",{month:"short",day:"numeric",hour:"2-digit",minute:"2-digit"}) : ""].filter(Boolean).join(" · ")}
              </span>
            </button>
          ))}
          {filtered.length === 0 && (
            <p className="dim">{resumeList.length === 0 ? "nothing to resume" : "no matches"}</p>
          )}
        </div>
        );
      })()}

      <div className="createrow-inner">
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && create()}
          placeholder={
            kind === "pi"
              ? "agent name (local pi, no tools)"
              : "new session name (optional)"
          }
          autoCapitalize="none"
        />
        <button
          className={"btn btn-primary" + (kind !== "shell" ? " btn-agent" : "")}
          onClick={create}
        >
          {kind === "shell" ? "new" : "agent"}
        </button>
      </div>
    </div>
  );
}

function Terminal({
  relay,
  session,
  onExit,
  onKilled,
}: {
  relay: Relay;
  session: SessionMeta;
  onExit: () => void;
  onKilled: () => void;
}) {
  const sessionId = session.id;
  const [panes, setPanes] = useState<Map<string, PaneSnap>>(new Map());
  const [layout, setLayout] = useState<Layout | null>(null);
  const [activePane, setActivePane] = useState("");
  const [conn, setConn] = useState("connecting…");
  const [geom, setGeom] = useState({ cols: 80, rows: 24 });
  const geomRef = useRef(geom);
  geomRef.current = geom;
  const wrapRef = useRef<HTMLDivElement | null>(null);
  // Real (invisible) input, not a focus-div: mobile keyboards only pop
  // up when a text field is focused — a div with tabIndex never brings
  // up the soft keyboard, so the demo was read-only on phones.
  const focusRef = useRef<HTMLInputElement | null>(null);
  const lastSeq = useRef<Map<string, number>>(new Map());
  const panesRef = useRef(panes);
  panesRef.current = panes;
  const [chatDraft, setChatDraft] = useState("");
  const [chatAttachments, setChatAttachments] = useState<string[]>([]);
  const [attachBrowse, setAttachBrowse] = useState<{ path: string; parent: string | null; dirs: string[]; files: string[] } | null>(null);
  const attachDirReqRef = useRef<string | null>(null);
  const chatScrollRef = useRef<HTMLDivElement | null>(null);
  // sticky-bottom chat: auto-follow new messages only while the user is
  // at the bottom; if they've scrolled up, leave the view alone until
  // they scroll back to the bottom
  const chatAtBottomRef = useRef(true);
  // chat scrollback pagination (chat panes): pending ChatHistory request +
  // scroll anchor captured at request time so the view doesn't jump when
  // older rows are prepended
  const chatHistReq = useRef<string | null>(null);
  const [chatLoadingOlder, setChatLoadingOlder] = useState(false);
  const chatScrollAnchor = useRef<{ y: number; contentH: number } | null>(null);
  const [history, setHistory] = useState<string[] | null>(null);
  // agent asks (Phase A2): the latest unresolved question for this
  // session; the card shows when its pane is the active one
  const [pendingAsk, setPendingAsk] = useState<AgentAskRequest | null>(null);
  const pendingAskRef = useRef(pendingAsk);
  pendingAskRef.current = pendingAsk;
  const [askSel, setAskSel] = useState<number[]>([]);
  const [askText, setAskText] = useState("");
  // "answered: …" rows appended to the visible chat list
  const [askNotes, setAskNotes] = useState<{ id: string; text: string }[]>([]);
  // frame-handler bookkeeping (refs so the onFrame closure is stale-safe):
  // choice labels per ask, asks we answered locally, notes already shown
  const askChoicesRef = useRef(new Map<string, string[]>());
  const answeredAskRef = useRef(new Set<string>());
  const askNoteIdsRef = useRef(new Set<string>());

  // an ask with choices may be sent with no selection (empty answer is
  // valid); with no choices the user must type text to unlock Send
  const askCanSend = !!(
    pendingAsk &&
    (askSel.length > 0 || askText.trim() !== "" || pendingAsk.choices.length > 0)
  );
  const askSend = () => {
    if (!pendingAsk || !askCanSend) return;
    relay.send({
      t: "AgentAskAnswer", ask_id: pendingAsk.ask_id,
      choices: askSel, text: askText.trim(),
    } as Frame);
    // mark locally so the broadcast AgentAskAnswer still appends the
    // "answered:" row even though the card is already cleared
    answeredAskRef.current.add(pendingAsk.ask_id);
    setPendingAsk(null);
  };
  // switching panes resets the card's selection/typing for that pane's ask
  useEffect(() => {
    const ask = pendingAskRef.current;
    setAskSel(
      ask && ask.pane === activePane && ask.suggested != null ? [ask.suggested] : []
    );
    setAskText("");
  }, [activePane]);

  useEffect(() => {
    const unlisten = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "HelloOk":
          setConn("online");
          break;
        case "Snapshot": {
          if (f.session !== sessionId) return;
          const m = new Map<string, PaneSnap>();
          lastSeq.current.clear();
          for (const p0 of f.panes) {
            const pad = [...p0.lines];
            while (pad.length < p0.rows) pad.push("");
            // chat panes: snapshot only carries the last CHAT_TAIL rows
            // (Attach.chat_limit) — merge in older rows we already paged in
            let chat = p0.chat ?? [];
            let chatHasMore = !!p0.chat_has_more;
            if (p0.kind === "forge-chat") {
              const merged = mergeChat(getCachedChat(sessionId, p0.id), chat, chatHasMore);
              chat = merged.msgs;
              chatHasMore = merged.hasMore;
              saveCachedChat(sessionId, p0.id, chat, chatHasMore);
            }
            m.set(p0.id, { ...p0, lines: pad, agentBusy: chat[chat.length - 1]?.role === "user", chat: p0.kind === "forge-chat" ? chat : p0.chat, chat_has_more: chatHasMore ? true : undefined });
            if (p0.seq) lastSeq.current.set(p0.id, p0.seq);
          }
          setPanes(m);
          setLayout(f.layout);
          setActivePane(f.active_pane);
          setConn("online");
          break;
        }
        case "Update": {
          if (f.session !== sessionId) return;
          const cur = panesRef.current.get(f.pane);
          if (!cur) return;
          const prevSeq = lastSeq.current.get(f.pane) ?? 0;
          if (prevSeq > 0 && f.seq !== prevSeq + 1) {
            if (f.seq <= prevSeq) break;
            lastSeq.current.delete(f.pane);
            relay.send({ t: "Attach", id: nextId(), client: "web", session: sessionId, chat_limit: CHAT_TAIL } as Frame);
            break;
          }
          lastSeq.current.set(f.pane, f.seq);
          const lines = cur.lines.slice();
          for (const [y, text] of f.rows_upd) {
            while (lines.length <= y) lines.push("");
            lines[y] = text;
          }
          const next = new Map(panesRef.current);
          next.set(f.pane, { ...cur, lines, cursor: f.cursor ?? cur.cursor });
          setPanes(next);
          break;
        }
        case "Chat": {
          if (f.session !== sessionId) break;
          const cur = panesRef.current.get(f.pane);
          if (!cur) break;
          const chat = f.reset ? [...(f.msgs ?? [])] : [...(cur.chat ?? [])];
          if (!f.reset) {
            for (const m of f.msgs ?? []) {
              const last = chat[chat.length - 1];
              if (!last || m.seq > last.seq) chat.push(m);
            }
          }
          const next = new Map(panesRef.current);
          next.set(f.pane, { ...cur, chat });
          setPanes(next);
          // keep the on-device cache in step: a reset (e.g. post-compact
          // history) wipes it, appends extend it
          const cached = getCachedChat(sessionId, f.pane) ?? { msgs: [], hasMore: false };
          saveCachedChat(
            sessionId, f.pane,
            f.reset ? chat : [...cached.msgs, ...chat.filter((m) => !cached.msgs.some((c) => c.seq === m.seq))],
            f.reset ? false : cached.hasMore,
          );
          break;
        }
        case "ChatHistoryOk": {
          // older chat rows paged in via scrollback — only act on OUR
          // request, only if the pane still exists
          if (chatHistReq.current !== f.req_id) break;
          chatHistReq.current = null;
          setChatLoadingOlder(false);
          chatScrollAnchor.current = null;
          const cur = panesRef.current.get(f.pane);
          if (!cur || f.msgs.length === 0) break;
          const existing = new Set((cur.chat ?? []).map((m) => m.seq));
          const add = f.msgs.filter((m) => !existing.has(m.seq));
          if (add.length === 0) break;
          const chat = [...add, ...(cur.chat ?? [])];
          const next = new Map(panesRef.current);
          next.set(f.pane, { ...cur, chat, chat_has_more: f.has_more });
          setPanes(next);
          saveCachedChat(sessionId, f.pane, chat, f.has_more);
          break;
        }
        case "Scrollback":
          setHistory(f.lines);
          break;
        case "Meta":
          if (f.kind === "exited" && f.session === sessionId) {
            // the session died under us (killed from the list page,
            // daily reset, another client) — leave the dead view
            setConn("session ended");
            clearSessionChat(sessionId);
            onKilled();
          }
          if (f.kind === "agent" && f.pane) {
            const cur = panesRef.current.get(f.pane);
            if (cur) {
              const next = new Map(panesRef.current);
              next.set(f.pane, { ...cur, agentBusy: f.status === "working" });
              setPanes(next);
            }
          }
          if (f.kind === "context" && f.pane && f.status) {
            const cur = panesRef.current.get(f.pane);
            if (cur) {
              const next = new Map(panesRef.current);
              next.set(f.pane, { ...cur, context: f.status });
              setPanes(next);
            }
          }
          break;
        case "Error":
          setConn(`error: ${f.message}`);
          break;
        case "DirListOk": {
          if (f.req_id === attachDirReqRef.current) {
            attachDirReqRef.current = null;
            setAttachBrowse({ path: f.path, parent: f.parent ?? null, dirs: f.dirs, files: f.files ?? [] });
          }
          break;
        }
        case "AgentAskRequest": {
          if (f.session !== sessionId) break;
          // a new ask replaces any pending one (incl. for the same pane)
          askChoicesRef.current.set(f.ask_id, f.choices);
          setPendingAsk(f);
          setAskSel(f.suggested != null ? [f.suggested] : []);
          setAskText("");
          break;
        }
        case "AgentAskAnswer": {
          // ignore answers we have no context for (answered by another
          // client/session, or an ask already superseded)
          if (pendingAskRef.current?.ask_id !== f.ask_id &&
              !answeredAskRef.current.has(f.ask_id)) break;
          if (askNoteIdsRef.current.has(f.ask_id)) break;
          askNoteIdsRef.current.add(f.ask_id);
          answeredAskRef.current.delete(f.ask_id);
          const labels = f.choices
            .map((i) => askChoicesRef.current.get(f.ask_id)?.[i])
            .filter((l): l is string => !!l)
            .join(", ");
          const text = f.text.trim();
          const shown = labels && text ? `${labels}, ${text}`
            : labels || text || "(empty)";
          if (pendingAskRef.current?.ask_id === f.ask_id) setPendingAsk(null);
          setAskNotes((prev) => [...prev, { id: f.ask_id, text: `answered: ${shown}` }]);
          break;
        }
      }
    });
    relay.onStatus = setConn;

    let gotSnap = false;
    const poke = () => {
      if (gotSnap) return;
      relay.send({ t: "Hello", id: nextId(), client: "web" } as Frame);
      relay.send({ t: "Attach", id: nextId(), client: "web", session: sessionId, chat_limit: CHAT_TAIL } as Frame);
      relay.send({
        t: "Resize", id: nextId(), client: "web", session: sessionId,
        cols: geomRef.current.cols, rows: geomRef.current.rows,
      } as Frame);
    };
    poke();
    const retryTimer = setInterval(poke, 3000);
    const resyncTimer = setInterval(() => {
      relay.send({ t: "Attach", id: nextId(), client: "web", session: sessionId, chat_limit: CHAT_TAIL } as Frame);
    }, 15000);
    setTimeout(() => focusRef.current?.focus(), 100);
    return () => {
      clearInterval(retryTimer);
      clearInterval(resyncTimer);
      relay.send({ t: "Detach", id: nextId(), client: "web" } as Frame);
      unlisten();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver((entries) => {
      const r = entries[0];
      const cols = Math.max(20, Math.floor(r.contentRect.width / (FS * 0.6)) - 1);
      const rows = Math.max(10, Math.floor(r.contentRect.height / LH) - 1);
      if (cols !== geomRef.current.cols || rows !== geomRef.current.rows) {
        setGeom({ cols, rows });
        relay.send({
          t: "Resize", id: nextId(), client: "web", session: sessionId,
          cols, rows,
        } as Frame);
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // chat scrollback: page in older rows when the user nears the top.
  // Captures the scroll position so the view doesn't jump when older
  // rows are prepended (applied in the post-render scroll effect).
  const loadOlder = (y: number, contentH: number) => {
    if (!activePane || chatHistReq.current) return;
    const snap = panesRef.current.get(activePane);
    const msgs = snap?.chat ?? [];
    if (msgs.length === 0 || !snap?.chat_has_more) return;
    const rid = nextId();
    chatHistReq.current = rid;
    setChatLoadingOlder(true);
    chatScrollAnchor.current = { y, contentH };
    relay.send({
      t: "ChatHistory", id: nextId(), client: "web",
      session: sessionId, pane: activePane, req_id: rid,
      limit: CHAT_PAGE, before: msgs[0].seq,
    } as Frame);
  };

  const send = (text: string) => {
    if (!activePane || text === "") return;
    relay.send({
      t: "Input", id: nextId(), client: "web",
      session: sessionId, pane: activePane, data: b64(text),
    } as Frame);
  };

  // attachment file browser (separate from the session-create dir browse)
  const attachBrowseDir = (path?: string) => {
    if (!relay) return;
    const rid = nextId();
    attachDirReqRef.current = rid;
    relay.send({ t: "DirList", id: nextId(), client: "web", req_id: rid, path } as Frame);
  };

  const openAttachPicker = () => {
    setAttachBrowse(null);
    attachBrowseDir(undefined); // default $HOME
  };

  const removeAttachment = (path: string) => {
    setChatAttachments(prev => prev.filter(p => p !== path));
  };

  const onKey = (e: React.KeyboardEvent) => {
    const k = e.key;
    // printable chars must never linger in the sink field (the
    // onChange path also clears, this covers desktop keydown-first)
    if (k.length === 1 && focusRef.current) focusRef.current.value = "";
    let data: string | null = null;
    if (k === "Enter") data = "\r";
    else if (k === "Backspace") data = "\u007f";
    else if (k === "Tab") data = "\t";
    else if (k === "Escape") data = "\x1b";
    else if (k === "ArrowUp") data = "\x1b[A";
    else if (k === "ArrowDown") data = "\x1b[B";
    else if (k === "ArrowRight") data = "\x1b[C";
    else if (k === "ArrowLeft") data = "\x1b[D";
    else if (k === "Home") data = "\x1b[H";
    else if (k === "End") data = "\x1b[F";
    else if (k === "PageUp") data = "\x1b[5~";
    else if (k === "PageDown") data = "\x1b[6~";
    else if (e.ctrlKey && k.length === 1) {
      const code = k.toUpperCase().charCodeAt(0) - 64;
      if (code >= 1 && code <= 26) data = String.fromCharCode(code);
    }
    if (data !== null) {
      e.preventDefault();
      send(data);
    } else if (k.length === 1 && !e.metaKey) {
      e.preventDefault();
      send(k);
    }
  };

  const rects = useMemo(
    () => (layout ? layoutRects(layout, 0, 0, geom.cols, geom.rows) : []),
    [layout, geom]
  );
  const activeSnap = activePane ? panes.get(activePane) : undefined;
  const chatMode = activeSnap?.kind === "forge-chat";
  const chatMsgs = activeSnap?.chat ?? [];
  // sticky-bottom chat: auto-follow new messages only while the user is
  // at the bottom; if they've scrolled up, leave the view alone until
  // they scroll back to the bottom
  useEffect(() => {
    const el = chatScrollRef.current;
    if (!el) return;
    const anchor = chatScrollAnchor.current;
    if (anchor) {
      // a scrollback page was just prepended: shift down by the new
      // content height so the view stays where the user left it
      chatScrollAnchor.current = null;
      const dy = el.scrollHeight - anchor.contentH;
      if (dy > 0) el.scrollTop = anchor.y + dy;
      return;
    }
    if (chatAtBottomRef.current) el.scrollTop = el.scrollHeight;
  });
  // switching to a different chat pane re-arms bottom-follow so the new
  // conversation opens at the newest message
  useEffect(() => {
    chatAtBottomRef.current = true;
  }, [activePane]);

  return (
    <div className="term-page">
      <div className="term-header">
        <button className="linkbtn" onClick={onExit}>‹ back</button>
        <span className="title-inline">{session.name}</span>
        <span className="conn-badge">{conn}</span>
        <span className="spacer" />
        {/* kill the whole session — the ⋯ menu on the list page only
            works from there; inside the session there was no way to
            end it (kill pane is disabled for single-pane sessions) */}
        <button
          className="keybtn keybtn-danger"
          title="kill session"
          onClick={() => {
            clearSessionChat(sessionId);
            relay.send({ t: "SessionsKill", session: sessionId } as Frame);
            onKilled();
          }}
        >
          kill session
        </button>
        {/* pane management: split / kill on the focused pane */}
        <button
          className="keybtn"
          title="split right"
          onClick={() =>
            relay.send({
              t: "PaneSplit", req_id: nextId(), session: sessionId, pane: activePane, dir: 1,
            } as Frame)
          }
        >
          split ⫞
        </button>
        <button
          className="keybtn"
          title="kill pane"
          disabled={panes.size < 2}
          onClick={() => {
            if (panes.size < 2) return;
            relay.send({ t: "PaneKill", session: sessionId, pane: activePane } as Frame);
          }}
        >
          kill pane
        </button>
        {panes.size > 1 &&
          [...panes.keys()].map((p, i) => (
            <button
              key={p}
              className={"keybtn" + (p === activePane ? " keybtn-on" : "")}
              onClick={() => {
                setActivePane(p);
                focusRef.current?.focus({ preventScroll: true });
                relay.send({ t: "SessionsSelect", session: sessionId, pane: p } as Frame);
              }}
            >
              {i + 1}
            </button>
          ))}
      </div>

      {chatMode ? (
        <div className="chat-wrap">
          <div className="dim" style={{ padding: "6px 10px 0", fontSize: "0.85rem", fontFamily: "var(--mono)" }}>
            {activeSnap?.context ?? ""}
            {activeSnap?.context ? <span> · /compact to compress</span> : null}
          </div>
          <div
            className="chat-list"
            ref={chatScrollRef}
            onScroll={(e) => {
              const el = e.currentTarget;
              const atBottom =
                el.scrollHeight - el.scrollTop - el.clientHeight < 24;
              chatAtBottomRef.current = atBottom;
              if (!atBottom && el.scrollTop < 60) loadOlder(el.scrollTop, el.scrollHeight);
            }}
          >
            {chatLoadingOlder && (
              <div className="dim" style={{ textAlign: 'center', padding: '4px 0', fontSize: '0.8rem' }}>
                loading older messages…
              </div>
            )}
            {!chatLoadingOlder && !activeSnap?.chat_has_more && chatMsgs.length > 0 && (
              <div className="dim" style={{ textAlign: 'center', fontSize: '0.75rem', opacity: 0.6 }}>
                — beginning of conversation —
              </div>
            )}
            {chatMsgs
              .filter((m: ChatMsg) => m.role === "tool" || m.text?.trim() !== "")
              .map((m, i) =>
                m.role === "tool" ? (
                  <details key={i} className="toolrow">
                    <summary>⚙ {m.tool_name || "tool"}{m.duration_ms != null ? ` · ${m.duration_ms}ms` : ""}{tsOf(m.created_at) ? ` · ${tsOf(m.created_at)}` : ""}</summary>
                    {m.tool_output && <pre className="toolout">{m.tool_output}</pre>}
                  </details>
                ) : (
                  <div key={i} className={"bubble " + (m.role === "user" ? "bubble-user" : "bubble-agent")}>
                    {m.attachments && m.attachments.length > 0 && (
                      <div className="attach-chips">
                        {m.attachments.map((a, j) => (
                          <span key={j} className="attach-chip">📎 {a.split('/').pop()}</span>
                        ))}
                      </div>
                    )}
                    {m.text}
                    {tsOf(m.created_at) && (
                      <span className="bubble-ts">{tsOf(m.created_at)}</span>
                    )}
                  </div>
                )
              )}
            {chatMsgs.length === 0 && <p className="dim">say something to the agent…</p>}
            {activeSnap?.agentBusy && <div className="bubble bubble-agent">● ● ●</div>}
            {askNotes.map((n) => (
              <div key={n.id} className="ask-note">{n.text}</div>
            ))}
          </div>
          {pendingAsk && pendingAsk.pane === activePane && (
            <div className="ask-card">
              <p className="ask-question">{pendingAsk.question}</p>
              {pendingAsk.choices.length > 0 && (
                <div className="ask-choices">
                  {pendingAsk.choices.map((c: string, i: number) => {
                    const on = askSel.includes(i);
                    return (
                      <button
                        key={i}
                        className={
                          "ask-choice" +
                          (on ? " ask-choice-on" : "") +
                          (pendingAsk.suggested === i ? " ask-choice-suggested" : "")
                        }
                        onClick={() =>
                          setAskSel((prev) =>
                            pendingAsk.multi
                              ? on
                                ? prev.filter((x) => x !== i)
                                : [...prev, i]
                              : [i]
                          )
                        }
                      >
                        <span className="ask-mark">
                          {on
                            ? pendingAsk.multi ? "☑" : "●"
                            : pendingAsk.multi ? "☐" : "○"}
                        </span>
                        <span className="ask-choice-label">{c}</span>
                        {pendingAsk.suggested === i && (
                          <span className="ask-sug">(suggested)</span>
                        )}
                      </button>
                    );
                  })}
                </div>
              )}
              {pendingAsk.free_text && (
                <input
                  className="ask-free-text"
                  type="text"
                  value={askText}
                  onChange={(e) => setAskText(e.target.value)}
                  placeholder="or type your own answer…"
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && askCanSend) askSend();
                  }}
                />
              )}
              <button
                className="btn btn-primary ask-send"
                disabled={!askCanSend}
                onClick={askSend}
              >
                Send
              </button>
            </div>
          )}
          <div className="chat-inputrow">
            {chatAttachments.length > 0 && (
              <div className="attach-chips" style={{ marginBottom: 4 }}>
                {chatAttachments.map((a) => (
                  <span key={a} className="attach-chip" onClick={() => removeAttachment(a)} style={{ cursor: 'pointer' }}>
                    📎 {a.split('/').pop()} ✕
                  </span>
                ))}
              </div>
            )}
            <div style={{ display: 'flex', gap: 6 }}>
              <button
                onClick={openAttachPicker}
                title="Attach file"
                style={{ background: 'transparent', border: '1px solid #444', borderRadius: 4, color: '#aaa', cursor: 'pointer', fontSize: '1rem', padding: '2px 8px' }}
              >📎</button>
              <input
                value={chatDraft}
                onChange={(e) => setChatDraft(e.target.value)}
                placeholder="message the agent"
                style={{ flex: 1 }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && (chatDraft.trim() || chatAttachments.length > 0)) {
                    const text = chatDraft.trim() || "(see attached files)";
                    if (text === "/compact") {
                      relay.send({
                        t: "ChatCompact", id: nextId(), client: "web",
                        session: sessionId, pane: activePane, req_id: `compact-${Date.now()}`,
                      } as Frame);
                    } else {
                      relay.send({
                        t: "ChatSend", id: nextId(), client: "web",
                        session: sessionId, pane: activePane, text,
                        attachments: chatAttachments.length > 0 ? chatAttachments : undefined,
                      } as Frame);
                    }
                    chatAtBottomRef.current = true;
                    setChatDraft("");
                    setChatAttachments([]);
                  }
                }}
              />
            </div>
          </div>
          {attachBrowse !== undefined && attachBrowse !== null && chatMode && (
            <div style={{ position: 'absolute', bottom: '60px', left: 10, right: 10, maxHeight: '40vh', overflow: 'auto', background: '#1a1a2e', border: '1px solid #444', borderRadius: 8, padding: 8, zIndex: 100 }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 4 }}>
                <span className="dim" style={{ fontSize: '0.8rem' }}>{attachBrowse.path}</span>
                <div style={{ display: 'flex', gap: 8 }}>
                  {attachBrowse.parent && (
                    <button onClick={() => attachBrowseDir(attachBrowse.parent!)} style={{ background: 'transparent', border: 'none', color: '#88f', cursor: 'pointer' }}>← up</button>
                  )}
                  <button onClick={() => { setAttachBrowse(null); }} style={{ background: 'transparent', border: 'none', color: '#f66', cursor: 'pointer' }}>✕ close</button>
                </div>
              </div>
              {attachBrowse.dirs.map((d) => (
                <div key={d} onClick={() => attachBrowseDir(`${attachBrowse.path}/${d}`)} style={{ cursor: 'pointer', padding: '2px 0', fontSize: '0.85rem' }}>
                  📁 {d}
                </div>
              ))}
              {attachBrowse.files.map((f) => (
                <div key={f} onClick={() => { setChatAttachments(prev => prev.includes(`${attachBrowse.path}/${f}`) ? prev : [...prev, `${attachBrowse.path}/${f}`]); }} style={{ cursor: 'pointer', padding: '2px 0', fontSize: '0.85rem', color: '#8f8' }}>
                  📄 {f}
                </div>
              ))}
            </div>
          )}
        </div>
      ) : (
        <div className="term-wrap" ref={wrapRef}>
          {rects.map((r) => {
            const p = panes.get(r.pane);
            return (
              <PaneView
                key={r.pane}
                pane={p ?? { id: r.pane, cols: r.w, rows: r.h, lines: [] }}
                rect={r}
                cols={geom.cols}
                rows={geom.rows}
                focused={r.pane === activePane}
                onSelect={() => {
                  setActivePane(r.pane);
                  focusRef.current?.focus({ preventScroll: true });
                  relay.send({
                    t: "SessionsSelect", session: sessionId, pane: r.pane,
                  } as Frame);
                }}
              />
            );
          })}
          {rects.length === 0 && (
            <div className="term-waiting">
              <p className="dim">waiting for snapshot… ({conn})</p>
            </div>
          )}
          {/* click/tap anywhere in the terminal area re-focuses the
              keystroke sink: closing the soft keyboard blurs it, and
              without this there's no way to get it back (on desktop
              nothing re-focuses after clicking the output text) */}
          <div
            className="term-capture"
            onTouchEnd={(e) => {
              // iOS: focus() must run in the touch handler, before the
              // synthetic mouse events fire, or the keyboard won't open
              e.preventDefault();
              focusRef.current?.focus({ preventScroll: true });
            }}
            onMouseDown={(e) => {
              // prevent the mousedown from stealing focus to body
              e.preventDefault();
              focusRef.current?.focus({ preventScroll: true });
            }}
          />
          <input
            ref={focusRef}
            className="term-focus"
            // real input: brings up the soft keyboard on mobile;
            // autocomplete attrs off, it's a terminal keystroke sink
            type="text"
            autoCapitalize="off"
            autoComplete="off"
            autoCorrect="off"
            spellCheck={false}
            aria-label="terminal input"
            // keep the field empty — keys are handled + cleared below
            value=""
            onChange={() => {
              // Android G-board style IMEs commit text without firing
              // keydown; forward whatever landed and clear
              const el = focusRef.current;
              if (el && el.value) {
                for (const ch of el.value) send(ch);
                el.value = "";
              }
            }}
            onKeyDown={onKey}
          />
        </div>
      )}

      {history !== null && (
        <div className="hist-wrap">
          <p>
            <b>scrollback ({history.length})</b>{" "}
            <button className="linkbtn" onClick={() => setHistory(null)}>close ✕</button>
          </p>
          <pre className="hist">{history.join("\n")}</pre>
        </div>
      )}

      {!chatMode && (
        <div className="term-keys">
          {[
            ["←", "\x1b[D"], ["↑", "\x1b[A"], ["↓", "\x1b[B"], ["→", "\x1b[C"],
            ["Enter", "\r"], ["Esc", "\x1b"], ["Tab", "\t"],
            ["Ctrl-C", "\x03"], ["Ctrl-D", "\x04"], ["Ctrl-L", "\x0c"],
          ].map(([label, seq]) => (
            <button key={label} className="keybtn" onClick={() => send(seq)}>
              {label}
            </button>
          ))}
          <button
            className="keybtn"
            onClick={() =>
              relay.send({
                t: "ScrollbackReq", id: nextId(), client: "web",
                session: sessionId, pane: activePane, offset: 0, limit: 2000,
              } as Frame)
            }
          >
            hist
          </button>
        </div>
      )}
    </div>
  );
}
