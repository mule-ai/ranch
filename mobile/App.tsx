import { useCallback, useEffect, useState, useRef} from "react";
import { useFonts } from "expo-font";
import {
  ActivityIndicator,
  Alert,
  BackHandler,
  FlatList,
  Keyboard,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { supabase } from "./lib/supabase";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import { Relay } from "./lib/relay";
import { Frame, ForgeSessionInfo, ProfileSummary, SessionMeta, nextId } from "./lib/frames";
import { LoginScreen, EmailFallback } from "./screens/Login";
import { MachinesScreen } from "./screens/Machines";
import { TerminalScreen } from "./screens/Terminal";
import { EditorScreen } from "./screens/Editor";
import { AgentsScreen } from "./screens/Agents";
import { WorkflowsScreen } from "./screens/Workflows";
import { TriggersScreen } from "./screens/Triggers";

type Machine = { id: string; name: string };

export default function App() {
  const insets = useSafeAreaInsets();
  const [fontsLoaded] = useFonts({
    "JetBrainsMono NF Mono": require("./assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf"),
  });
  // screens: login → (email fallback lives on login screen) → machines → sessions → terminal
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [machine, setMachine] = useState<Machine | null>(null);
  const [relay, setRelay] = useState<Relay | null>(null);
  const [sessions, setSessions] = useState<SessionMeta[] | null>(null);
  const [attached, setAttached] = useState<SessionMeta | null>(null);
  // M10: file editor (browse/read/write files on this machine)
  // Phase B: agent builder (profiles) + a profile awaiting launch
  // Bottom tabs while a machine is picked: sessions | files | agents |
  // automation (the old top-bar action pile — five text buttons
  // crammed next to the title — is unusable on a phone)
  const [tab, setTab] = useState<"sessions" | "files" | "agents" | "automation">("sessions");
  const [pendingProfile, setPendingProfile] = useState<ProfileSummary | null>(null);
  // hot daemon upgrade in flight (button shows progress, err shows result)
  const [upgrading, setUpgrading] = useState(false);
  const upgradingRef = useRef(false);
  upgradingRef.current = upgrading;
  const [newName, setNewName] = useState("");
  // session kind for the create row: shell or forge (agent running pi)
  const [newKind, setNewKind] = useState<"shell" | "forge" | "pi">("shell");
  // forge session picker (resume); null = closed
  const [resumeList, setResumeList] = useState<ForgeSessionInfo[] | null>(null);
  // local-pi working dir (null = daemon default $HOME)
  const [piDir, setPiDir] = useState<string | null>(null);
  // directory browser sheet for picking piDir
  const [dirBrowse, setDirBrowse] = useState<
    { path: string; parent: string | null; dirs: string[] } | null
  >(null);
  // refs mirror the states for the frame handler, whose effect never
  // re-runs (deps: machine id only) — stale closure otherwise
  const nameRef = useRef(newName);
  nameRef.current = newName;
  const kindRef = useRef(newKind);
  kindRef.current = newKind;
  const resumeReqRef = useRef<string | null>(null);
  const dirReqRef = useRef<string | null>(null);
  const browseDir = (r: Relay | null, path?: string) => {
    if (!r) return;
    const rid = nextId();
    dirReqRef.current = rid;
    r.send({ t: "DirList", id: nextId(), client: "mobile", req_id: rid, path } as Frame);
  };
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

  // Android back gesture/button: mirror the on-screen back control for
  // whatever view is on top. Each full-screen sub-screen (editor, agents,
  // workflows, triggers, terminal) installs its own handler for its
  // internal back (including unsaved-changes confirms); this one covers
  // the app-level views and must yield when a sub-screen is up.
  const backReqRef = useRef(false);
  backReqRef.current = !!(tab !== "sessions" || attached);
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", () => {
      if (backReqRef.current) return false; // let the sub-screen's handler run
      if (attached !== null) setAttached(null);
      else if (machine) setMachine(null);
      else return false; // login/machines root: default (exit/background)
      return true;
    });
    return () => sub.remove();
  }, [attached, machine]);

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

  // Phase B: launch an agent profile from the builder — create a forge
  // session bound to the profile; SessionsAck attaches (existing flow).
  useEffect(() => {
    if (!relay || !pendingProfile) return;
    relay.send({
      t: "SessionsCreate", req_id: nextId(), name: pendingProfile.name,
      kind: "forge", profile_id: pendingProfile.id,
    } as Frame);
    setPendingProfile(null);
  }, [relay, pendingProfile]);


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
            // hot upgrade: the daemon is back with the new binary
            setUpgrading(false);
            break;
          case "SessionsAck":
            // created via the sessions screen → attach to it
            setSessions((prev) => [
              ...(prev ?? []),
              { id: f.session, name: nameRef.current || f.session.slice(0, 8), kind: kindRef.current, active_pane: f.pane, panes: [f.pane] },
            ]);
            setNewName("");
            break;
          case "ForgeListOk":
            if (f.req_id === resumeReqRef.current) {
              resumeReqRef.current = null;
              setResumeList(f.sessions);
            }
            break;
          case "DirListOk":
            if (f.req_id === dirReqRef.current) {
              dirReqRef.current = null;
              setDirBrowse({
                path: f.path,
                parent: f.parent ?? null,
                dirs: f.dirs,
              });
            }
            break;
          case "Error":
            setErr(f.message);
            break;
        }
      });
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

  // Terminal is attached full-screen above the tab bar; everything else
  // lives under the tabs.
  if (attached && relay) {
    return (
      <TerminalScreen
        relay={relay}
        sessionId={attached.id}
        sessionName={attached.name}
        onExit={() => {
          setAttached(null);
          relay.send({ t: "Hello", id: nextId(), client: "mobile" } as Frame);
        }}
      />
    );
  }

  if (!machine) {
    return null;
  }

  if (!relay) {
    return (
      <View style={s.center}>
        <ActivityIndicator color="#4ade80" />
      </View>
    );
  }

  const upgrade = () =>
    Alert.alert(
      "hot upgrade",
      "restart the daemon in place? Sessions and agent panes are kept alive (zero downtime).",
      [
        { text: "cancel", style: "cancel" },
        {
          text: "upgrade",
          onPress: () => {
            setUpgrading(true);
            relay.send({ t: "Upgrade" } as Frame);
            // if the daemon doesn't come back in 30s, surface it
            setTimeout(() => {
              if (upgradingRef.current) {
                setUpgrading(false);
                setErr("daemon did not come back after upgrade — try refresh");
              }
            }, 30000);
          },
        },
      ],
    );

  return (
    <View style={[s.wrap, { paddingBottom: kbHeight + 64 + insets.bottom }]}>
      <View style={s.header}>
        <Pressable onPress={() => setMachine(null)} hitSlop={8}>
          <Text style={s.back}>‹ machines</Text>
        </Pressable>
        <Text style={s.title} numberOfLines={1}>{machine.name}</Text>
        <Pressable onPress={() =>
          Alert.alert(machine.name, undefined, [
            { text: "cancel", style: "cancel" },
            { text: upgrading ? "upgrading…" : "hot upgrade", onPress: upgrade },
            { text: "switch machine", onPress: () => setMachine(null) },
          ])
        } hitSlop={8}>
          <Text style={s.menu}>⋯</Text>
        </Pressable>
      </View>
      {tab === "sessions" && (
        <>
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
        {pendingProfile !== null && relay && (
          <View style={{ paddingVertical: 8 }}>
            <Text style={s.dim}>
              launching agent "{pendingProfile.name}"…
            </Text>
          </View>
        )}
        {resumeList !== null && (
          <View style={[s.resumeSheet]}>
            <Text style={s.rowTitle}>forge sessions</Text>
            <FlatList
              data={resumeList}
              keyExtractor={(item) => item.id}
              style={{ maxHeight: 320 }}
              renderItem={({ item }) => (
                <Pressable
                  style={s.row}
                  onPress={() => {
                    setResumeList(null);
                    relay?.send({
                      t: "SessionsCreate", req_id: nextId(), kind: "forge",
                      forge_session: item.id,
                    } as Frame);
                  }}
                >
                  <Text style={s.rowTitle} numberOfLines={1}>
                    {item.title || item.id.slice(0, 8)}
                  </Text>
                  <Text style={s.dim}>
                    {item.ended ? "ended" : "active"} ·{" "}
                    {item.updated ? item.updated.slice(0, 16).replace("T", " ") : ""}
                  </Text>
                </Pressable>
              )}
              ListEmptyComponent={<Text style={s.dim}>nothing to resume</Text>}
            />
            <Pressable style={s.kindChip} onPress={() => setResumeList(null)}>
              <Text style={s.kindText}>close</Text>
            </Pressable>
          </View>
        )}
        <View style={s.kindRow}>
          {(["shell", "forge", "pi"] as const).map((k) => (
            <Pressable
              key={k}
              style={[s.kindChip, newKind === k && s.kindChipOn]}
              onPress={() => setNewKind(k)}
            >
              <Text style={[s.kindText, newKind === k && s.kindTextOn]}>
                {k === "forge" ? "agent" : k}
              </Text>
            </Pressable>
          ))}
          <Pressable
            style={s.kindChip}
            onPress={() => {
              const rid = nextId();
              resumeReqRef.current = rid;
              relay?.send({ t: "ForgeList", id: nextId(), client: "mobile", req_id: rid } as Frame);
            }}
          >
            <Text style={s.kindText}>resume…</Text>
          </Pressable>
        </View>
        <View style={s.newRow}>
          {newKind === "pi" && (
            <View style={s.kindRow}>
              <Pressable
                style={[s.kindChip, s.dirChip]}
                onPress={() => browseDir(relay, piDir ?? undefined)}
              >
                <Text style={s.kindText} numberOfLines={1}>
                  dir: {piDir ?? "$HOME"}
                </Text>
              </Pressable>
            </View>
          )}
          {dirBrowse !== null && (
            <View style={s.resumeSheet}>
              <Text style={s.rowTitle} numberOfLines={1}>
                {dirBrowse.path}
              </Text>
              <FlatList
                data={dirBrowse.dirs}
                keyExtractor={(item) => item}
                style={{ maxHeight: 300 }}
                renderItem={({ item }) => (
                  <Pressable
                    style={s.row}
                    onPress={() => browseDir(relay, dirBrowse.path + "/" + item)}
                  >
                    <Text style={s.rowTitle}>{item}/</Text>
                  </Pressable>
                )}
                ListEmptyComponent={<Text style={s.dim}>no subdirectories</Text>}
              />
              <View style={s.kindRow}>
                {dirBrowse.parent !== null && (
                  <Pressable
                    style={s.kindChip}
                    onPress={() => browseDir(relay, dirBrowse.parent ?? undefined)}
                  >
                    <Text style={s.kindText}>up…</Text>
                  </Pressable>
                )}
                <Pressable
                  style={[s.kindChip, s.kindChipOn]}
                  onPress={() => {
                    setPiDir(dirBrowse.path);
                    setDirBrowse(null);
                  }}
                >
                  <Text style={[s.kindText, s.kindTextOn]}>use this dir</Text>
                </Pressable>
                <Pressable style={s.kindChip} onPress={() => setDirBrowse(null)}>
                  <Text style={s.kindText}>cancel</Text>
                </Pressable>
              </View>
            </View>
          )}
          <TextInput
            style={s.input}
            value={newName}
            onChangeText={setNewName}
            placeholder={
              newKind === "forge"
                ? "agent name (lab forge)"
                : newKind === "pi"
                  ? "local pi name (optional)"
                  : "new session name (optional)"
            }
            placeholderTextColor="#4b5563"
            autoCapitalize="none"
          />
          <Pressable
            style={[s.sendBtn, newKind === "forge" && s.sendBtnAgent]}
            onPress={() =>
              relay?.send({
                t: "SessionsCreate", req_id: nextId(), name: newName || undefined,
                kind: newKind,
                cwd: newKind === "pi" ? (piDir ?? undefined) : undefined,
              } as Frame)
            }
          >
            <Text style={s.btnText}>{newKind === "forge" ? "agent" : "new"}</Text>
          </Pressable>
        </View>
        </>
      )}
      {tab === "files" && (
        <EditorScreen relay={relay} onExit={() => setTab("sessions")} />
      )}
      {tab === "agents" && (
        <AgentsScreen
          relay={relay}
          onExit={() => setTab("sessions")}
          onLaunch={(p) => {
            setPendingProfile(p);
            setTab("sessions");
          }}
        />
      )}
      {tab === "automation" && <AutomationScreen relay={relay} />}
      <TabBar tab={tab} onPick={setTab} />
    </View>
  );
}

/** Bottom tab bar: the machine's five surfaces, always visible and
 * thumb-reachable. The terminal (attached) renders above it full-screen. */
function TabBar({
  tab,
  onPick,
}: {
  tab: "sessions" | "files" | "agents" | "automation";
  onPick: (t: "sessions" | "files" | "agents" | "automation") => void;
}) {
  // sit above the Android gesture-bar/home-pill area, not under it
  const insets = useSafeAreaInsets();
  const tabs = [
    { id: "sessions" as const, label: "sessions", icon: "□" },
    { id: "files" as const, label: "files", icon: "≡" },
    { id: "agents" as const, label: "agents", icon: "◆" },
    { id: "automation" as const, label: "automation", icon: "⏱" },
  ];
  return (
    <View style={[s.tabbar, { paddingBottom: 10 + insets.bottom }]}>
      {tabs.map((t) => (
        <Pressable key={t.id} style={s.tab} onPress={() => onPick(t.id)} hitSlop={4}>
          <Text style={[s.tabIcon, tab === t.id && s.tabIconOn]}>{t.icon}</Text>
          <Text style={[s.tabLabel, tab === t.id && s.tabLabelOn]}>{t.label}</Text>
        </Pressable>
      ))}
    </View>
  );
}

/** Workflows + triggers under one roof (one automation domain, two
 * lists) — a segmented control instead of two top-bar entries. */
function AutomationScreen({ relay }: { relay: Relay }) {
  const [seg, setSeg] = useState<"workflows" | "triggers">("workflows");
  return (
    <View style={{ flex: 1 }}>
      <View style={s.segRow}>
        <Pressable
          style={[s.segBtn, seg === "workflows" && s.segBtnOn]}
          onPress={() => setSeg("workflows")}
        >
          <Text style={[s.segText, seg === "workflows" && s.segTextOn]}>workflows</Text>
        </Pressable>
        <Pressable
          style={[s.segBtn, seg === "triggers" && s.segBtnOn]}
          onPress={() => setSeg("triggers")}
        >
          <Text style={[s.segText, seg === "triggers" && s.segTextOn]}>triggers</Text>
        </Pressable>
      </View>
      {seg === "workflows" ? (
        <WorkflowsScreen relay={relay} onExit={() => setSeg("triggers")} />
      ) : (
        <TriggersScreen relay={relay} onExit={() => setSeg("workflows")} />
      )}
    </View>
  );
}

/** Install the given handler as the Android hardware/gesture back
 * handler while mounted (the swipe-back gesture fires the same
 * `hardwareBackPress` event as the button). Returns true from `handler`
 * consumes the event; false lets the app-level handler (or the system)
 * take it. */
export function useAndroidBack(handler: () => boolean) {
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", handler);
    return () => sub.remove();
  }, [handler]);
}

const s = StyleSheet.create({
  center: { flex: 1, backgroundColor: "#101014", justifyContent: "center", alignItems: "center" },
  wrap: { flex: 1, backgroundColor: "#101014", paddingTop: 60, paddingHorizontal: 16 },
  // sessions tab body (fills the space under the header)
  tabBody: { flex: 1 },
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
  dirChip: { flex: 1, alignItems: "flex-start" },
  resumeSheet: {
    backgroundColor: "#16161c", borderRadius: 12, padding: 10,
    borderWidth: 1, borderColor: "#2a2a34", maxHeight: 420,
  },
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
  // header machine menu + bottom tab bar
  menu: { color: "#9ca3af", fontSize: 20, paddingHorizontal: 6, fontWeight: "700" },
  tabbar: {
    position: "absolute", bottom: 0, left: 0, right: 0,
    flexDirection: "row", backgroundColor: "#121218",
    borderTopWidth: 1, borderTopColor: "#1f2430",
    paddingTop: 6, paddingBottom: 10,
  },
  tab: { flex: 1, alignItems: "center", gap: 2 },
  tabIcon: { color: "#6b7280", fontSize: 18, lineHeight: 22 },
  tabIconOn: { color: "#4ade80" },
  tabLabel: { color: "#6b7280", fontSize: 11 },
  tabLabelOn: { color: "#4ade80", fontWeight: "600" },
  // automation segmented control
  segRow: { flexDirection: "row", gap: 8, paddingHorizontal: 16, paddingTop: 64, paddingBottom: 6 },
  segBtn: {
    borderWidth: 1, borderColor: "#374151", borderRadius: 999,
    paddingHorizontal: 14, paddingVertical: 5,
  },
  segBtnOn: { borderColor: "#4ade80", backgroundColor: "rgba(74,222,128,0.12)" },
  segText: { color: "#9ca3af", fontSize: 13 },
  segTextOn: { color: "#4ade80", fontWeight: "600" },
});
