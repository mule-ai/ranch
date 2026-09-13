// Workflows screen (Phase C): browse/run/delete mule workflows through
// the daemon's mule proxy; tap "run" to start a job — a workflow pane
// (session wf-<id>) opens and streams the run; attach via the session
// list. Editing lives on the web surface.
import { useCallback, useEffect, useState } from "react";
import {
  Alert,
  ActivityIndicator,
  BackHandler,
  FlatList,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { Relay } from "../lib/relay";
import { Frame, MuleAgent, WorkflowSummary, nextId } from "../lib/frames";

// Android back gesture/button = the on-screen back control.
function useAndroidBack(handler: () => boolean) {
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", handler);
    return () => sub.remove();
  }, [handler]);
}

type Props = { relay: Relay; onExit: () => void; embedded?: boolean };

export function WorkflowsScreen({ relay, onExit, embedded }: Props) {
  const [workflows, setWorkflows] = useState<WorkflowSummary[] | null>(null);
  const [running, setRunning] = useState<string | null>(null);
  // create form (full editing stays web-side; mobile create covers the
  // common "single agent step" workflow)
  const [creating, setCreating] = useState(false);

  const refresh = useCallback(() => {
    relay.send({ t: "WorkflowList", id: nextId(), client: "mobile", req_id: nextId() } as Frame);
  }, [relay]);

  // back gesture/button = the on-screen back control
  useAndroidBack(useCallback(() => {
    onExit();
    return true;
  }, [onExit]));

  useEffect(() => {
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "WorkflowListOk":
          setWorkflows(f.workflows);
          setRunning(null);
          break;
        case "WorkflowRunOk":
          setRunning(null);
          refresh();
          break;
        case "Error":
          setRunning(null);
          break;
      }
    });
    refresh();
    return un;
  }, [relay, refresh]);

  return (
    <View style={s.wrap}>
      {!embedded && (
      <View style={s.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={s.back}>‹ sessions</Text>
        </Pressable>
        <Text style={s.title}>workflows</Text>
        <View style={{ width: 70 }} />
      </View>
      )}
      {workflows === null ? (
        <ActivityIndicator color="#4ade80" style={{ marginTop: 32 }} />
      ) : (
        <FlatList
          data={workflows}
          keyExtractor={(w) => w.id}
          contentContainerStyle={{ paddingBottom: 40 }}
          ListEmptyComponent={<Text style={s.dim}>No mule workflows configured.</Text>}
          renderItem={({ item }) => (
            <View style={s.row}>
              <Text style={s.rowTitle}>{item.name}</Text>
              {item.description ? <Text style={s.dim}>{item.description}</Text> : null}
              <View style={s.btnRow}>
                <Pressable
                  style={[s.chip, s.chipGo, running === item.id && { opacity: 0.5 }]}
                  onPress={() => {
                    setRunning(item.id);
                    relay.send({
                      t: "WorkflowRun", id: nextId(), client: "mobile",
                      req_id: nextId(), workflow: item.id,
                    } as Frame);
                  }}
                  disabled={running === item.id}
                >
                  <Text style={s.chipGoText}>{running === item.id ? "starting…" : "run"}</Text>
                </Pressable>
                <Pressable
                  style={s.chip}
                  onLongPress={() =>
                    Alert.alert("delete workflow", `delete "${item.name}"?`, [
                      { text: "cancel", style: "cancel" },
                      {
                        text: "delete",
                        style: "destructive",
                        onPress: () =>
                          relay.send({ t: "WorkflowDelete", id: nextId(), client: "mobile", req_id: nextId(), workflow: item.id } as Frame),
                      },
                    ])
                  }
                >
                  <Text style={[s.chipText, { color: "#f87171" }]}>delete</Text>
                </Pressable>
              </View>
            </View>
          )}
        />
      )}
      <Pressable style={s.newBtn} onPress={() => setCreating(true)}>
        <Text style={s.newBtnText}>new workflow</Text>
      </Pressable>
      {creating && <WorkflowCreateForm relay={relay} onDone={() => setCreating(false)} />}
    </View>
  );
}

/// Minimal workflow create: name/description + agent steps. The web
/// surface has the full editor (wasm steps, reordering, JSON config
/// polish); this covers "run this prompt through agent X".
function WorkflowCreateForm({
  relay,
  onDone,
}: {
  relay: Relay;
  onDone: () => void;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  // mule agents: the one step runs agent 0 (Default) unless picked
  const [agents, setAgents] = useState<MuleAgent[]>([]);
  const [agentId, setAgentId] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const rid = nextId();
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "MuleAgentsOk" && f.req_id === rid) {
        un();
        setAgents(f.agents);
        if (f.agents.length > 0 && agentId === "") {
          const def = f.agents.find((a) => a.name === "Default") ?? f.agents[0];
          setAgentId(def.id);
        }
      }
    });
    relay.send({ t: "MuleAgents", id: nextId(), client: "mobile", req_id: rid } as Frame);
    return un;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [relay]);

  const save = () => {
    if (!name.trim()) {
      setErr("name is required");
      return;
    }
    if (!agentId) {
      setErr("pick an agent for the step");
      return;
    }
    setBusy(true);
    const rid = nextId();
    relay.send({
      t: "WorkflowPut", id: nextId(), client: "mobile", req_id: rid,
      workflow_id: null,
      draft: {
        name: name.trim(),
        description: description.trim() || null,
        is_async: false,
        steps: [
          {
            step_order: 0,
            type: "agent",
            agent_id: agentId,
            config: {},
          },
        ],
      },
    } as unknown as Frame);
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "WorkflowPutOk" && f.req_id === rid) {
        un();
        onDone();
      } else if (f.t === "Error" && f.req_id === rid) {
        un();
        setErr(f.message);
        setBusy(false);
      }
    });
  };

  return (
    <View style={s.sheet}>
      <Text style={s.rowTitle}>new workflow</Text>
      <Text style={s.label}>name</Text>
      <TextInput style={s.input} value={name} onChangeText={setName} autoCapitalize="none" placeholder="nightly triage" placeholderTextColor="#4b5563" />
      <Text style={s.label}>description (optional)</Text>
      <TextInput style={s.input} value={description} onChangeText={setDescription} autoCapitalize="none" />
      <Text style={s.label}>agent (runs each time; prompt comes at run time)</Text>
      {agents.length === 0 ? (
        <Text style={s.dim}>loading agents…</Text>
      ) : (
        <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 6 }}>
          {agents.map((a) => (
            <Pressable key={a.id} style={[s.chip, agentId === a.id && s.chipOn]} onPress={() => setAgentId(a.id)}>
              <Text style={[s.chipText, agentId === a.id && s.chipTextOn]} numberOfLines={1}>{a.name}</Text>
            </Pressable>
          ))}
        </View>
      )}
      {err !== "" && <Text style={s.errText}>{err}</Text>}
      <View style={{ flexDirection: "row", gap: 8, marginTop: 8 }}>
        <Pressable style={[s.newBtn, { flex: 1, marginVertical: 0 }, busy && { opacity: 0.5 }]} onPress={save} disabled={busy}>
          <Text style={s.newBtnText}>{busy ? "saving…" : "create"}</Text>
        </Pressable>
        <Pressable style={[s.newBtn, { flex: 1, marginVertical: 0, backgroundColor: "#1f2430" }]} onPress={onDone}>
          <Text style={s.newBtnText}>cancel</Text>
        </Pressable>
      </View>
    </View>
  );
}

const s = StyleSheet.create({
  wrap: { flex: 1, backgroundColor: "#101014" },
  newBtn: {
    backgroundColor: "#16a34a", borderRadius: 8, paddingVertical: 12,
    alignItems: "center", marginVertical: 8,
  },
  newBtnText: { color: "#fff", fontWeight: "700" },
  chip: {
    borderWidth: 1, borderColor: "#374151", borderRadius: 999,
    paddingHorizontal: 12, paddingVertical: 4,
  },
  chipOn: { borderColor: "#4ade80", backgroundColor: "rgba(74,222,128,0.12)" },
  chipText: { color: "#9ca3af", fontSize: 13 },
  chipTextOn: { color: "#4ade80", fontSize: 13, fontWeight: "600" },
  sheet: {
    backgroundColor: "#16161c", borderRadius: 12, padding: 12,
    borderWidth: 1, borderColor: "#2a2a34", marginBottom: 12, gap: 6,
  },
  label: { color: "#9ca3af", fontSize: 12 },
  input: {
    backgroundColor: "#1a1b23", borderRadius: 8, paddingHorizontal: 12,
    paddingVertical: 8, color: "#f3f4f6", fontSize: 14,
  },
  errText: { color: "#f87171", fontSize: 13 },
  header: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", marginBottom: 12 },
  back: { color: "#4ade80", width: 70 },
  title: { color: "#f3f4f6", fontWeight: "700", fontSize: 17 },
  row: { paddingVertical: 12, borderBottomWidth: 1, borderBottomColor: "#1f2430", gap: 2 },
  rowTitle: { color: "#f3f4f6", fontSize: 17, fontWeight: "600" },
  dim: { color: "#6b7280", fontSize: 13 },
  btnRow: { flexDirection: "row", gap: 8, marginTop: 8 },
  chipGo: { borderColor: "#4ade80" },
  chipGoText: { color: "#4ade80", fontSize: 13, fontWeight: "700" },
});
