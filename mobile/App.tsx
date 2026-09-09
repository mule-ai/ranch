import { useCallback, useEffect, useState, useRef} from "react";
import { useFonts } from "expo-font";
import {
  ActivityIndicator,
  Alert,
  FlatList,
  Keyboard,
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
  // screens: login → (email fallback lives on login screen) → machines → sessions → terminal
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [machine, setMachine] = useState<Machine | null>(null);
  const [relay, setRelay] = useState<Relay | null>(null);
  const [sessions, setSessions] = useState<SessionMeta[] | null>(null);
  const [attached, setAttached] = useState<SessionMeta | null>(null);
  const [newName, setNewName] = useState("");
  // session kind for the create row: shell or forge (agent running pi)
  const [newKind, setNewKind] = useState<"shell" | "forge">("shell");
  // refs mirror the states for the frame handler, whose effect never
  // re-runs (deps: machine id only) — stale closure otherwise
  const nameRef = useRef(newName);
  nameRef.current = newName;
  const kindRef = useRef(newKind);
  kindRef.current = newKind;
  const [err, setErr] = useState("");
  // keyboard inset: edge-to-edge Android doesn't lift bottom inputs, so
  // pad the sessions screen by the measured keyboard height
  const [kbHeight, setKbHeight] = useState(0);
  useEffect(() => {
    const show = Keyboard.addListener("keyboardDidShow", (e) =>
      setKbHeight(e.endCoordinates?.height ?? 0)
    );
    const hide = Keyboard.addListener("keyboardDidHide", () => setKbHeight(0));
    return () => {
      show.remove();
      hide.remove();
    };
  }, []);

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
    let unlisten: (() => void) | null = null;
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
      unlisten = r.onFrame((f: Frame) => {
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
              { id: f.session, name: nameRef.current || f.session.slice(0, 8), kind: kindRef.current, active_pane: f.pane, panes: [f.pane] },
            ]);
            setNewName("");
            break;
          case "Error":
            setErr(f.message);
            break;
        }
      });
      try {
        await r.join();
      } catch (e: any) {
        setSessions([]);
        setErr(e.message ?? "realtime connection failed");
        return;
      }
    })();
    return () => {
      unlisten?.();
      if (retryTimer) clearInterval(retryTimer);
      r?.leave();
      setRelay(null);
      setSessions(null);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [machine?.id]);

  if (!fontsLoaded) {
    return (
      <View style={{ flex: 1, backgroundColor: "#101014", justifyContent: "center", alignItems: "center" }}>
        <ActivityIndicator color="#4ade80" />
      </View>
    );
  }

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
      <View style={[s.wrap, { paddingBottom: kbHeight }]}>
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
              <Pressable
                style={s.row}
                onPress={() => setAttached(item)}
                onLongPress={() =>
                  Alert.alert("kill session", `kill "${item.name}"?`, [
                    { text: "cancel", style: "cancel" },
                    {
                      text: "kill",
                      style: "destructive",
                      onPress: () => {
                        relay?.send({ t: "SessionsKill", session: item.id } as Frame);
                        setSessions(
                          (sessions ?? []).filter((x) => x.id !== item.id)
                        );
                      },
                    },
                  ])
                }
              >
                <Text style={s.rowTitle}>{item.name}</Text>
                <Text style={s.dim}>
                  {item.kind} · {item.panes.length} pane{item.panes.length === 1 ? "" : "s"}
                </Text>
              </Pressable>
            )}
          />
        )}
        <View style={s.kindRow}>
          {(["shell", "forge"] as const).map((k) => (
            <Pressable
              key={k}
              style={[s.kindChip, newKind === k && s.kindChipOn]}
              onPress={() => setNewKind(k)}
            >
              <Text style={[s.kindText, newKind === k && s.kindTextOn]}>
                {k === "forge" ? "agent" : "shell"}
              </Text>
            </Pressable>
          ))}
        </View>
        <View style={s.newRow}>
          <TextInput
            style={s.input}
            value={newName}
            onChangeText={setNewName}
            placeholder={newKind === "forge" ? "agent name (runs pi)" : "new session name (optional)"}
            placeholderTextColor="#4b5563"
            autoCapitalize="none"
          />
          <Pressable
            style={[s.sendBtn, newKind === "forge" && s.sendBtnAgent]}
            onPress={() => relay?.send({ t: "SessionsCreate", req_id: nextId(), name: newName || undefined, kind: newKind } as Frame)}
          >
            <Text style={s.btnText}>{newKind === "forge" ? "agent" : "new"}</Text>
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
  kindRow: { flexDirection: "row", gap: 8, paddingBottom: 4 },
  kindChip: {
    borderWidth: 1, borderColor: "#374151", borderRadius: 999,
    paddingHorizontal: 12, paddingVertical: 4,
  },
  kindChipOn: { borderColor: "#4ade80", backgroundColor: "rgba(74,222,128,0.12)" },
  kindText: { color: "#9ca3af", fontSize: 13 },
  kindTextOn: { color: "#4ade80", fontSize: 13, fontWeight: "600" },
  sendBtnAgent: { backgroundColor: "#a855f7" },
  input: {
    flex: 1, backgroundColor: "#1a1b23", borderRadius: 8,
    paddingHorizontal: 12, paddingVertical: 8, color: "#f3f4f6",
  },
  sendBtn: { backgroundColor: "#16a34a", borderRadius: 8, paddingHorizontal: 16, justifyContent: "center" },
  btnText: { color: "#fff", fontWeight: "700" },
});
