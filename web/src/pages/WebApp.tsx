// The live client: login → machine picker → sessions (full management:
// create shell/agent/pi, rename, kill, dir picker, forge resume, hot
// upgrade) → terminal (split panes, keys, scrollback, agent chat).
import { useEffect, useMemo, useRef, useState, useCallback } from "react";
import { Relay } from "../lib/relay";
import {
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
import { Login } from "./Login";

type Machine = { id: string; name: string; last_seen_at: string | null };

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
    return <MachinePicker onPick={setMachine} onSignOut={() => supabase.auth.signOut()} />;
  return <MachineClient machine={machine} onBack={() => setMachine(null)} />;
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
  const [sessions, setSessions] = useState<SessionMeta[] | null>(null);
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
        if (r && Date.now() - r.lastInboundAt() > 15000) {
          r.ensureAlive(0); // 15s without ANY inbound frame -> force re-join
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
      />
    );

  return (
    <div className="page narrow">
      <p className="rowline">
        <button className="linkbtn" onClick={onBack}>‹ machines</button>
        <span className="title-inline">{machine.name}</span>
        <span className="conn-badge">{conn}</span>
      </p>
      {err !== "" && <p className="err">{err}</p>}

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

      <CreateRow relay={relay} />
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

function CreateRow({ relay }: { relay: Relay | null }) {
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
  const [resumeList, setResumeList] = useState<{ id: string; title: string; updated: string; ended?: string | null }[] | null>(null);
  const dirReqRef = useRef<string | null>(null);
  const resumeReqRef = useRef<string | null>(null);

  // frame listener for DirListOk / ForgeListOk
  useEffect(() => {
    if (!relay) return;
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "DirListOk" && f.req_id === dirReqRef.current) {
        dirReqRef.current = null;
        setDirBrowse({ path: f.path, parent: f.parent ?? null, dirs: f.dirs });
      } else if (f.t === "ForgeListOk" && f.req_id === resumeReqRef.current) {
        resumeReqRef.current = null;
        setResumeList(f.sessions);
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
              resumeReqRef.current = rid;
              relay.send({ t: "ForgeList", id: nextId(), client: "web", req_id: rid } as Frame);
            }}
          >
            resume…
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

      {resumeList !== null && (
        <div className="sheet">
          <b>forge sessions</b>
          {resumeList.map((item) => (
            <button
              key={item.id}
              className="machrow"
              onClick={() => {
                setResumeList(null);
                relay?.send({
                  t: "SessionsCreate", req_id: nextId(), kind: "forge",
                  forge_session: item.id,
                } as Frame);
              }}
            >
              <span className="machname">{item.title || item.id.slice(0, 8)}</span>
              <span className="dim">
                {item.ended ? "ended" : "active"} · {item.updated?.slice(0, 16).replace("T", " ") ?? ""}
              </span>
            </button>
          ))}
          {resumeList.length === 0 && <p className="dim">nothing to resume</p>}
          <button className="chip" onClick={() => setResumeList(null)}>close</button>
        </div>
      )}

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
}: {
  relay: Relay;
  session: SessionMeta;
  onExit: () => void;
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
  const focusRef = useRef<HTMLDivElement | null>(null);
  const lastSeq = useRef<Map<string, number>>(new Map());
  const panesRef = useRef(panes);
  panesRef.current = panes;
  const [chatDraft, setChatDraft] = useState("");
  const chatScrollRef = useRef<HTMLDivElement | null>(null);
  // sticky-bottom chat: auto-follow new messages only while the user is
  // at the bottom; if they've scrolled up, leave the view alone until
  // they scroll back to the bottom
  const chatAtBottomRef = useRef(true);
  const [history, setHistory] = useState<string[] | null>(null);

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
            const chat = p0.chat ?? [];
            m.set(p0.id, { ...p0, lines: pad, agentBusy: chat[chat.length - 1]?.role === "user" });
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
            relay.send({ t: "Attach", id: nextId(), client: "web", session: sessionId } as Frame);
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
          break;
        }
        case "Scrollback":
          setHistory(f.lines);
          break;
        case "Meta":
          if (f.kind === "exited") setConn("session ended");
          if (f.kind === "agent" && f.pane) {
            const cur = panesRef.current.get(f.pane);
            if (cur) {
              const next = new Map(panesRef.current);
              next.set(f.pane, { ...cur, agentBusy: f.status === "working" });
              setPanes(next);
            }
          }
          break;
        case "Error":
          setConn(`error: ${f.message}`);
          break;
      }
    });
    relay.onStatus = setConn;

    let gotSnap = false;
    const poke = () => {
      if (gotSnap) return;
      relay.send({ t: "Hello", id: nextId(), client: "web" } as Frame);
      relay.send({ t: "Attach", id: nextId(), client: "web", session: sessionId } as Frame);
      relay.send({
        t: "Resize", id: nextId(), client: "web", session: sessionId,
        cols: geomRef.current.cols, rows: geomRef.current.rows,
      } as Frame);
    };
    poke();
    const retryTimer = setInterval(poke, 3000);
    const resyncTimer = setInterval(() => {
      relay.send({ t: "Attach", id: nextId(), client: "web", session: sessionId } as Frame);
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

  const send = (text: string) => {
    if (!activePane || text === "") return;
    relay.send({
      t: "Input", id: nextId(), client: "web",
      session: sessionId, pane: activePane, data: b64(text),
    } as Frame);
  };

  const onKey = (e: React.KeyboardEvent) => {
    const k = e.key;
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
    if (el && chatAtBottomRef.current) el.scrollTop = el.scrollHeight;
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
                focusRef.current?.focus();
                relay.send({ t: "SessionsSelect", session: sessionId, pane: p } as Frame);
              }}
            >
              {i + 1}
            </button>
          ))}
      </div>

      {chatMode ? (
        <div className="chat-wrap">
          <div
            className="chat-list"
            ref={chatScrollRef}
            onScroll={(e) => {
              const el = e.currentTarget;
              chatAtBottomRef.current =
                el.scrollHeight - el.scrollTop - el.clientHeight < 24;
            }}
          >
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
                    {m.text}
                    {tsOf(m.created_at) && (
                      <span className="bubble-ts">{tsOf(m.created_at)}</span>
                    )}
                  </div>
                )
              )}
            {chatMsgs.length === 0 && <p className="dim">say something to the agent…</p>}
            {activeSnap?.agentBusy && <div className="bubble bubble-agent">● ● ●</div>}
          </div>
          <div className="chat-inputrow">
            <input
              value={chatDraft}
              onChange={(e) => setChatDraft(e.target.value)}
              placeholder="message the agent"
              onKeyDown={(e) => {
                if (e.key === "Enter" && chatDraft.trim()) {
                  relay.send({
                    t: "ChatSend", id: nextId(), client: "web",
                    session: sessionId, pane: activePane, text: chatDraft.trim(),
                  } as Frame);
                  chatAtBottomRef.current = true; // our own send ⇒ follow
                  setChatDraft("");
                }
              }}
            />
          </div>
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
                  focusRef.current?.focus();
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
          <div ref={focusRef} tabIndex={0} className="term-focus" onKeyDown={onKey} />
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
