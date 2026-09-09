import { useCallback, useEffect, useState } from "react";
import { useFonts } from "expo-font";
import {
  ActivityIndicator,
  FlatList,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { supabase } from "./lib/supabase";
import { Relay } from "./lib/relay";
import { Frame, SessionMeta, nextId } from "./lib/frames";
import { LoginScreen, EmailFallback } from "./screens/Login";
import { MachinesScreen } from "./screens/Machines";
import { TerminalScreen } from "./screens/Terminal";

type Machine = { id: string; name: string };

export default function App() {
  const [fontsLoaded] = useFonts({
    "JetBrainsMono NF Mono": require("./assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf"),
  });
  if (!fontsLoaded) {
    return (
      <View style={{ flex: 1, backgroundColor: "#101014", justifyContent: "center", alignItems: "center" }}>
        <ActivityIndicator color="#4ade80" />
      </View>
    );
  }
  // screens: login → (email fallback lives on login screen) → machines → sessions → terminal
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [machine, setMachine] = useState<Machine | null>(null);
  const [relay, setRelay] = useState<Relay | null>(null);
  const [sessions, setSessions] = useState<SessionMeta[] | null>(null);
  const [attached, setAttached] = useState<SessionMeta | null>(null);
  const [newName, setNewName] = useState("");
  const [err, setErr] = useState("");

  // auth state
  useEffect(() => {
    supabase.auth.getSession().then(({ data }) => setAuthed(!!data.session));
    const { data: sub } = supabase.auth.onAuthStateChange((event: string, session) => {
      if (event === "SIGNED_OUT") setAuthed(false);
      else if (session) setAuthed(true);
    });
    return () => sub.subscription.unsubscribe();
  }, []);

  const signedIn = useCallback(() => setAuthed(true), []);

  // when a machine is picked: open relay, hello, list sessions.
  // Hello is retried every 3s until the daemon answers — the daemon's
  // relay reconnects with backoff after network blips, and a Hello sent
  // during that window is simply lost.
  useEffect(() => {
    if (!machine) return;
    let r: Relay | null = null;
    let retryTimer: ReturnType<typeof setInterval> | null = null;
    (async () => {
      setSessions(null);
      setErr("");
      r = new Relay(machine.id);
      let gotHello = false;
      const hello = () => r?.send({ t: "Hello", id: nextId(), client: "mobile" } as Frame);
      r.onReady = () => {
        // initial join and every reconnect: (re)request the session list
        gotHello = false;
        hello();
        if (retryTimer) clearInterval(retryTimer);
        retryTimer = setInterval(() => {
          if (!gotHello) hello();
        }, 3000);
      };
      r.onFrame = (f: Frame) => {
        switch (f.t) {
          case "HelloOk":
            gotHello = true;
            if (retryTimer) {
              clearInterval(retryTimer);
              retryTimer = null;
            }
            setSessions(f.sessions);
            break;
          case "SessionsAck":
            // created via the sessions screen → attach to it
            setSessions((prev) => [
              ...(prev ?? []),
              { id: f.session, name: newName || f.session.slice(0, 8), kind: "shell", active_pane: f.pane, panes: [f.pane] },
            ]);
            setNewName("");
            break;
          case "Error":
            setErr(f.message);
            break;
        }
      };
      setRelay(r);
      try {
        await r.join();
      } catch (e: any) {
        setSessions([]);
        setErr(e.message ?? "realtime connection failed");
        return;
      }
    })();
    return () => {
      if (retryTimer) clearInterval(retryTimer);
      r?.leave();
      setRelay(null);
      setSessions(null);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [machine?.id]);

  if (authed === null) {
    return (
      <View style={s.center}>
        <ActivityIndicator color="#4ade80" />
      </View>
    );
  }

  if (!authed) {
    return (
      <View style={{ flex: 1 }}>
        <LoginScreen onSignedIn={signedIn} />
        <View style={{ position: "absolute", bottom: 40, left: 0, right: 0, alignItems: "center" }}>
          <EmailFallback onSignedIn={signedIn} />
        </View>
      </View>
    );
  }

  if (!machine) {
    return <MachinesScreen onPick={(m) => setMachine(m)} />;
  }

  if (!attached) {
    return (
      <View style={s.wrap}>
        <View style={s.header}>
          <Pressable onPress={() => setMachine(null)} hitSlop={8}>
            <Text style={s.back}>‹ machines</Text>
          </Pressable>
          <Text style={s.title}>{machine.name}</Text>
          <View style={{ width: 70 }} />
        </View>
        {err !== "" && <Text style={s.err}>{err}</Text>}
        {sessions === null ? (
          <ActivityIndicator color="#4ade80" style={{ marginTop: 32 }} />
        ) : (
          <FlatList
            data={sessions}
            keyExtractor={(x) => x.id}
            contentContainerStyle={{ paddingBottom: 120 }}
            ListEmptyComponent={
              <Text style={s.dim}>No sessions. Create one below.</Text>
            }
            renderItem={({ item }) => (
              <Pressable style={s.row} onPress={() => setAttached(item)}>
                <Text style={s.rowTitle}>{item.name}</Text>
                <Text style={s.dim}>
                  {item.kind} · {item.panes.length} pane{item.panes.length === 1 ? "" : "s"}
                </Text>
              </Pressable>
            )}
          />
        )}
        <View style={s.newRow}>
          <TextInput
            style={s.input}
            value={newName}
            onChangeText={setNewName}
            placeholder="new session name (optional)"
            placeholderTextColor="#4b5563"
            autoCapitalize="none"
          />
          <Pressable
            style={s.sendBtn}
            onPress={() => relay?.send({ t: "SessionsCreate", req_id: nextId(), name: newName || undefined } as Frame)}
          >
            <Text style={s.btnText}>new</Text>
          </Pressable>
        </View>
      </View>
    );
  }

  return relay ? (
    <TerminalScreen
      relay={relay}
      sessionId={attached.id}
      sessionName={attached.name}
      onExit={() => {
        setAttached(null);
        relay.send({ t: "Hello", id: nextId(), client: "mobile" } as Frame);
      }}
    />
  ) : null;
}

const s = StyleSheet.create({
  center: { flex: 1, backgroundColor: "#101014", justifyContent: "center", alignItems: "center" },
  wrap: { flex: 1, backgroundColor: "#101014", paddingTop: 60, paddingHorizontal: 16 },
  header: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", marginBottom: 12 },
  back: { color: "#4ade80", width: 70 },
  title: { color: "#f3f4f6", fontWeight: "700", fontSize: 17 },
  row: {
    paddingVertical: 14, borderBottomWidth: 1, borderBottomColor: "#1f2430", gap: 2,
  },
  rowTitle: { color: "#f3f4f6", fontSize: 17, fontWeight: "600" },
  dim: { color: "#6b7280", fontSize: 13 },
  err: { color: "#f87171", marginBottom: 8 },
  newRow: { flexDirection: "row", gap: 8, paddingBottom: 30, paddingTop: 8 },
  input: {
    flex: 1, backgroundColor: "#1a1b23", borderRadius: 8,
    paddingHorizontal: 12, paddingVertical: 8, color: "#f3f4f6",
  },
  sendBtn: { backgroundColor: "#16a34a", borderRadius: 8, paddingHorizontal: 16, justifyContent: "center" },
  btnText: { color: "#fff", fontWeight: "700" },
});
