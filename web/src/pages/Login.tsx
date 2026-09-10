// Sign-in: Google OAuth (redirect flow) + email/password fallback.
// The redirect comes back to this same page (PKCE, detectSessionInUrl).
import { useState } from "react";
import { supabase } from "../lib/supabase";

export function Login({ onSignedIn }: { onSignedIn: () => void }) {
  const [busy, setBusy] = useState(false);
  const [email, setEmail] = useState("");
  const [pw, setPw] = useState("");
  const [err, setErr] = useState("");

  const google = async () => {
    setErr("");
    setBusy(true);
    try {
      // Plain origin+pathname (no hash): Supabase appends ?code=… to this
      // on the way back, and a trailing fragment would garble it. The
      // app re-routes to #/app once the session is detected.
      const { error } = await supabase.auth.signInWithOAuth({
        provider: "google",
        options: { redirectTo: window.location.origin + window.location.pathname },
      });
      if (error) throw error;
      // browser navigates away; nothing else to do
    } catch (e: any) {
      setErr(e.message ?? "sign-in failed");
      setBusy(false);
    }
  };

  const password = async () => {
    setErr("");
    setBusy(true);
    try {
      const { error } = await supabase.auth.signInWithPassword({ email, password: pw });
      if (error) throw error;
      onSignedIn();
    } catch (e: any) {
      setErr(e.message ?? "sign-in failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <div className="logo">🤠 ranch</div>
      <p className="dim">sign in to reach your machines</p>
      {busy ? (
        <p className="dim">…</p>
      ) : (
        <>
          <button className="btn btn-primary" onClick={google}>
            Sign in with Google
          </button>
          <div className="pwrow">
            <input
              placeholder="email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              autoCapitalize="none"
            />
            <input
              placeholder="password"
              type="password"
              value={pw}
              onChange={(e) => setPw(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && password()}
            />
            <button className="btn btn-ghost" onClick={password}>
              Sign in
            </button>
          </div>
        </>
      )}
      {err !== "" && <p className="err">{err}</p>}
    </div>
  );
}
