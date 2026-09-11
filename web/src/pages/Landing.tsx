import { useEffect, useState } from "react";
import { Link } from "../lib/router";
import { Release, fetchLatest } from "../lib/releases";
import { demoAvailable, signInDemo } from "../lib/demo";

export function Landing() {
  const [rel, setRel] = useState<Release | null>(null);
  const [demoBusy, setDemoBusy] = useState(false);
  const [demoErr, setDemoErr] = useState("");
  useEffect(() => {
    fetchLatest().then(setRel);
  }, []);

  const startDemo = async () => {
    setDemoErr("");
    setDemoBusy(true);
    try {
      await signInDemo();
      window.location.hash = "/app"; // hash nav — plain /app 404s on Pages
    } catch (e: any) {
      setDemoErr(e.message ?? "demo sign-in failed");
      setDemoBusy(false);
    }
  };

  return (
    <div className="page">
      <section className="hero">
        <div className="hero-mark">🤠</div>
        <h1>ranch</h1>
        <p className="tagline">
          A personal terminal multiplexer with a cloud relay.
          Long-lived sessions on your machine — from your terminal,
          your phone, or this page.
        </p>
        <div className="hero-actions">
          <Link to="/download" className="btn btn-primary">Download</Link>
          <Link to="/app" className="btn btn-ghost">Open web app →</Link>
          {demoAvailable && (
            <button
              className="btn btn-ghost"
              onClick={startDemo}
              disabled={demoBusy}
            >
              {demoBusy ? "starting…" : "Try the live demo"}
            </button>
          )}
        </div>
        {demoErr !== "" && <p className="err">{demoErr}</p>}
        <pre className="hero-term" aria-hidden>
{`$ ranch new work
✓ session 'work' created (pane 1)
$ ranch attach work
┌─ work ─ pane 1 ────────────────┐
│ $ cargo build --release        │
│    Compiling ranch-daemon      │
└── 80×24 ───────── 2 sessions ──┘`}
        </pre>
      </section>

      <section className="features">
        <div className="feature">
          <h3>Sessions that outlive everything</h3>
          <p>
            Detach, close the laptop, kill the app — panes keep running.
            Hot upgrades re-exec the daemon without dropping a single
            shell or agent child.
          </p>
        </div>
        <div className="feature">
          <h3>tmux, but reachable from anywhere</h3>
          <p>
            <code>Ctrl-B</code> splits, panes, sessions — the bindings you
            know. The difference: your phone and any browser are also
            first-class clients.
          </p>
        </div>
        <div className="feature">
          <h3>Agents are first-class panes</h3>
          <p>
            Forge and local-<code>pi</code> agent sessions render as
            conversations, not raw terminal dumps. Chat from the couch;
            the tool calls stay visible.
          </p>
        </div>
        <div className="feature">
          <h3>Cloud relay, RLS-gated</h3>
          <p>
            One private Supabase Realtime channel per machine. Row-level
            security pins every topic to the owner. Local activity never
            leaves the machine.
          </p>
        </div>
      </section>

      <section className="strip">
        <div className="strip-inner">
          <div>
            <b>{rel?.version ?? "latest"}</b>
            <span> APK + Linux x86_64 binaries, built by CI</span>
          </div>
          <Link to="/download" className="btn btn-primary">Get ranch</Link>
        </div>
      </section>

      <footer>
        open source ·{" "}
        <a href="https://github.com/mule-ai/ranch" target="_blank" rel="noreferrer">
          github.com/mule-ai/ranch
        </a>
      </footer>
    </div>
  );
}
