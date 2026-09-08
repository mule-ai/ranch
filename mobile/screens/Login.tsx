import { useEffect, useState } from "react";
import {
  ActivityIndicator,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import * as WebBrowser from "expo-web-browser";
import { makeRedirectUri } from "expo-auth-session";
import { supabase } from "../lib/supabase";

WebBrowser.maybeCompleteAuthSession();

export function LoginScreen({ onSignedIn }: { onSignedIn: () => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  // Complete OAuth when the app is reopened via deep link (dev.ranch.app://callback)
  useEffect(() => {
    supabase.auth.getSession().then(({ data }) => {
      if (data.session) onSignedIn();
    });
    const { data: sub } = supabase.auth.onAuthStateChange((event: string) => {
      if (event === "SIGNED_IN" || event === "TOKEN_REFRESHED") onSignedIn();
    });
    return () => sub.subscription.unsubscribe();
  }, [onSignedIn]);

  const google = async () => {
    setErr("");
    setBusy(true);
    try {
      // In Expo Go: exp://…; in a dev-client/standalone build: dev.ranch.app://callback
      // (both are allow-listed in the Supabase auth URL config)
      const redirectTo = makeRedirectUri({ native: "dev.ranch.app://callback" });
      const { data, error } = await supabase.auth.signInWithOAuth({
        provider: "google",
        options: { redirectTo, skipBrowserRedirect: true },
      });
      if (error || !data?.url) throw error ?? new Error("no oauth url");
      const res = await WebBrowser.openAuthSessionAsync(data.url, redirectTo);
      if (res.type === "success" && res.url) {
        // Supabase returns tokens in the URL fragment; hand the URL to the SDK
        const url = new URL(res.url.replace("#", "?"));
        const params = url.searchParams;
        if (params.get("access_token")) {
          await supabase.auth.setSession({
            access_token: params.get("access_token")!,
            refresh_token: params.get("refresh_token") ?? "",
          });
          onSignedIn();
        } else {
          // PKCE code flow
          const code = params.get("code");
          if (code) {
            await supabase.auth.exchangeCodeForSession(code);
            onSignedIn();
          } else {
            // deep link when the app was cold-started — getSession on resume
            const { data: s } = await supabase.auth.getSession();
            if (s.session) onSignedIn();
            else setErr("sign-in did not complete");
          }
        }
      } else if (res.type !== "dismiss") {
        setErr("sign-in cancelled");
      }
    } catch (e: any) {
      setErr(e.message ?? "sign-in failed");
    } finally {
      setBusy(false);
    }
  };

  // TODO: email+password fallback lives in App.tsx for dev without Google redirect config
  return (
    <View style={styles.wrap}>
      <Text style={styles.logo}>🤠 ranch</Text>
      <Text style={styles.sub}>your machines, from your pocket</Text>
      {busy ? (
        <ActivityIndicator color="#4ade80" style={{ marginTop: 32 }} />
      ) : (
        <Pressable style={styles.btn} onPress={google}>
          <Text style={styles.btnText}>Sign in with Google</Text>
        </Pressable>
      )}
      {err !== "" && <Text style={styles.err}>{err}</Text>}
    </View>
  );
}

export function EmailFallback({ onSignedIn }: { onSignedIn: () => void }) {
  // Password sign-in (owner@ranch.local during development; Google flow above
  // is the real path once a dev-client build exists for the scheme redirect)
  const [email, setEmail] = useState("");
  const [pw, setPw] = useState("");
  const [err, setErr] = useState("");
  return (
    <View style={{ gap: 8, marginTop: 24 }}>
      <Text style={{ color: "#6b7280", fontSize: 12 }}>dev: password sign-in</Text>
      <TextInput placeholder="email" placeholderTextColor="#4b5563" value={email}
        onChangeText={setEmail} autoCapitalize="none" keyboardType="email-address"
        style={styles.input} />
      <TextInput placeholder="password" placeholderTextColor="#4b5563" value={pw}
        onChangeText={setPw} secureTextEntry style={styles.input} />
      <Pressable
        style={[styles.btn, { marginTop: 4 }]}
        onPress={async () => {
          setErr("");
          const { error } = await supabase.auth.signInWithPassword({ email, password: pw });
          if (error) setErr(error.message);
          else onSignedIn();
        }}
      >
        <Text style={styles.btnText}>Sign in</Text>
      </Pressable>
      {err !== "" && <Text style={styles.err}>{err}</Text>}
    </View>
  );
}

const styles = StyleSheet.create({
  wrap: { flex: 1, backgroundColor: "#101014", justifyContent: "center", alignItems: "center", padding: 24 },
  logo: { fontSize: 40, marginBottom: 8 },
  sub: { color: "#6b7280", marginBottom: 40 },
  btn: { backgroundColor: "#1f2937", borderRadius: 12, paddingVertical: 14, paddingHorizontal: 32 },
  btnText: { color: "#e5e7eb", fontWeight: "700", fontSize: 16 },
  err: { color: "#f87171", marginTop: 12 },
  input: {
    backgroundColor: "#1a1b23", borderRadius: 8, paddingHorizontal: 12,
    paddingVertical: 10, color: "#f3f4f6", width: 260,
  },
});
