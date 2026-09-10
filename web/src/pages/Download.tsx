import { useEffect, useState } from "react";
import { Link } from "../lib/router";
import { Release, fetchLatest, distFile } from "../lib/releases";

export function Download() {
  const [rel, setRel] = useState<Release | null | "loading">("loading");
  useEffect(() => {
    fetchLatest().then((r) => setRel(r));
  }, []);

  return (
    <div className="page narrow">
      <h1>Download</h1>
      <p className="dim">
        Built by CI straight from the repo. The APK is signed with a
        personal keystore; the Linux binaries are static enough to drop in
        <code>~/.local/bin</code>.
      </p>

      {rel === "loading" && <p className="dim">checking for releases…</p>}

      {rel === null && (
        <div className="card">
          <p>
            No published release yet — the first CI build lands here
            automatically. Until then, build from source:
          </p>
          <pre>{`git clone https://github.com/mule-ai/ranch
cd ranch && make setup && make install`}</pre>
        </div>
      )}

      {rel && rel !== "loading" && (
        <>
          <div className="card">
            <h2>Android app</h2>
            <p className="dim">v{rel.version} · built {rel.built.slice(0, 10)}</p>
            <a className="btn btn-primary" href={distFile(rel.apk)} download>
              Download APK
            </a>
            {rel.sha256_apk && (
              <p className="sha">sha256 {rel.sha256_apk.slice(0, 32)}…</p>
            )}
          </div>

          <div className="card">
            <h2>Linux x86_64 binary</h2>
            <p className="dim">
              One static binary — <code>ranch</code> (CLI + TUI + daemon).
              No runtime deps; drop it in <code>~/.local/bin</code> and go.
              <code>ln -s ranch ranchd</code> (or <code>ranch daemon</code>)
              runs the daemon.
            </p>
            <a className="btn btn-primary" href={distFile(rel.linux)} download>
              Download tarball
            </a>
            {rel.sha256_linux && (
              <p className="sha">sha256 {rel.sha256_linux.slice(0, 32)}…</p>
            )}
          </div>
        </>
      )}

      {rel && rel !== "loading" && (
        <div className="card">
          <h2>Install (Linux)</h2>
          <pre>{`tar xzf ranch-linux-x86_64.tar.gz
cd ranch-linux-x86_64
mkdir -p ~/.local/bin
install ranch ~/.local/bin/ranch
ln -sf ranch ~/.local/bin/ranchd`}</pre>
          <p className="dim">
            Then sign in and register this machine as a daemon host:
          </p>
          <pre>{`ranch login          # Google sign-in (once per device)
ranch register       # register with your ranch account
ranch daemon         # or: make service / run ranchd under systemd`}</pre>
          <p className="dim">
            The daemon keeps every session's panes alive — leave it running.
            <code>ranch</code> (in a TTY) opens the interactive dashboard;
            the Android app and{" "}
            <a href="https://mule-ai.github.io/ranch/#/app">this site's web app</a>{" "}
            reach the same sessions through the cloud relay.
          </p>
        </div>
      )}

      <div className="card">
        <h2>From source</h2>
        <pre>{`git clone https://github.com/mule-ai/ranch
cd ranch
make setup        # one-time: Zig + ghostty + libghostty-vt
make install      # ~/.local/bin/ranch (+ ranchd symlink)
make service      # systemd user unit (optional)

ranch upgrade     # later: hot-upgrade the running daemon, zero downtime`}</pre>
        <p className="dim">
          Needs Rust (stable) + Zig 0.16; JDK 17 + the Android SDK only
          if building the APK locally.
        </p>
      </div>

      <p>
        <Link to="/">← back</Link>
      </p>
    </div>
  );
}
