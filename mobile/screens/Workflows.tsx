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
  View,
} from "react-native";
import { Relay } from "../lib/relay";
import { Frame, WorkflowSummary, nextId } from "../lib/frames";

// Android back gesture/button = the on-screen back control.
function useAndroidBack(handler: () => boolean) {
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", handler);
    return () => sub.remove();
  }, [handler]);
}

type Props = { relay: Relay; onExit: () => void };

export function WorkflowsScreen({ relay, onExit }: Props) {
  const [workflows, setWorkflows] = useState<WorkflowSummary[] | null>(null);
  const [running, setRunning] = useState<string | null>(null);

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
      <View style={s.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={s.back}>‹ sessions</Text>
        </Pressable>
        <Text style={s.title}>workflows</Text>
        <View style={{ width: 70 }} />
      </View>
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
    </View>
  );
}

const s = StyleSheet.create({
  wrap: { flex: 1, backgroundColor: "#101014", paddingTop: 60, paddingHorizontal: 16 },
  header: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", marginBottom: 12 },
  back: { color: "#4ade80", width: 70 },
  title: { color: "#f3f4f6", fontWeight: "700", fontSize: 17 },
  row: { paddingVertical: 12, borderBottomWidth: 1, borderBottomColor: "#1f2430", gap: 2 },
  rowTitle: { color: "#f3f4f6", fontSize: 17, fontWeight: "600" },
  dim: { color: "#6b7280", fontSize: 13 },
  btnRow: { flexDirection: "row", gap: 8, marginTop: 8 },
  chip: {
    borderWidth: 1, borderColor: "#374151", borderRadius: 999,
    paddingHorizontal: 12, paddingVertical: 4,
  },
  chipText: { color: "#9ca3af", fontSize: 13 },
  chipGo: { borderColor: "#4ade80" },
  chipGoText: { color: "#4ade80", fontSize: 13, fontWeight: "700" },
});
