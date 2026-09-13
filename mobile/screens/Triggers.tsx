// Triggers screen (Phase D, mobile parity): list cron/event triggers
// with last-run status; run-now + enable/disable. Trigger creation/
// editing lives on the web surface (forms on a phone are the web's job).
import { useCallback, useEffect, useRef, useState } from "react";
import {
  ActivityIndicator,
  Alert,
  BackHandler,
  FlatList,
  TextInput,
  Pressable,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { Relay } from "../lib/relay";
import { Frame, TriggerRow, nextId } from "../lib/frames";

// Android back gesture/button = the on-screen back control.
function useAndroidBack(handler: () => boolean) {
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", handler);
    return () => sub.remove();
  }, [handler]);
}

type Props = { relay: Relay; onExit: () => void; embedded?: boolean };

export function TriggersScreen({ relay, onExit, embedded }: Props) {
  const [triggers, setTriggers] = useState<TriggerRow[] | null>(null);
  // create form (edit stays web-side; create here covers the common case)
  const [creating, setCreating] = useState(false);
  const workflowsRef = useRef<{ id: string; name: string }[]>([]);

  const refresh = useCallback(() => {
    relay.send({ t: "TriggerList", id: nextId(), client: "mobile", req_id: nextId() } as Frame);
  }, [relay]);

  // back gesture/button = the on-screen back control
  useAndroidBack(useCallback(() => {
    onExit();
    return true;
  }, [onExit]));

  useEffect(() => {
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "TriggerListOk":
          setTriggers(f.triggers as unknown as TriggerRow[]);
          break;
        case "WorkflowListOk":
          workflowsRef.current = (f.workflows as unknown as { id: string; name: string }[]) ?? [];
          break;
        case "TriggerDeleteOk":
        case "TriggerPutOk":
          setCreating(false);
          refresh();
          break;
        case "TriggerFired":
          refresh();
          break;
      }
    });
    refresh();
    // workflow list (for the create form's workflow picker)
    relay.send({ t: "WorkflowList", id: nextId(), client: "mobile", req_id: nextId() } as Frame);
    return un;
  }, [relay, refresh]);

  const preview = (t: TriggerRow) => {
    if (t.kind === "cron" && t.spec.cron) {
      const f = t.spec.cron.trim().split(/\s+/);
      if (f.length === 5) {
        if (f[1] === "*") return `hourly at :${f[0]}`;
        if (f[0] === "0") return `daily at ${f[1].padStart(2, "0")}:00`;
        return `cron ${t.spec.cron}`;
      }
      return t.spec.cron;
    }
    if (t.kind === "webhook") return "webhook";
    return t.spec.event ?? "event";
  };

  return (
    <View style={s.wrap}>
      {!embedded && (
      <View style={s.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={s.back}>‹ sessions</Text>
        </Pressable>
        <Text style={s.title}>triggers</Text>
        <View style={{ width: 70 }} />
      </View>
      )}
      {triggers === null ? (
        <ActivityIndicator color="#4ade80" style={{ marginTop: 32 }} />
      ) : (
        <FlatList
          data={triggers}
          keyExtractor={(t) => t.id ?? ""}
          contentContainerStyle={{ paddingBottom: 40 }}
          ListEmptyComponent={
            <Text style={s.dim}>No triggers yet.</Text>
          }
          renderItem={({ item: t }) => (
            <View style={s.row}>
              <Text style={s.rowTitle}>{t.name}</Text>
              <Text style={s.dim}>
                {preview(t)}
                {t.enabled ? "" : " · disabled"}
                {t.last_run ? ` · last ${t.last_run.status}` : ""}
              </Text>
              <View style={s.btnRow}>
                <Pressable
                  style={[s.chip, s.chipGo]}
                  onPress={() =>
                    relay.send({ t: "TriggerRun", id: nextId(), client: "mobile", req_id: nextId(), trigger: t.id! } as Frame)
                  }
                >
                  <Text style={s.chipGoText}>run now</Text>
                </Pressable>
                <Pressable
                  style={s.chip}
                  onPress={() =>
                    relay.send({
                      t: "TriggerPut", id: nextId(), client: "mobile", req_id: nextId(),
                      trigger_id: t.id, trigger: { ...t, enabled: !t.enabled },
                    } as unknown as Frame)
                  }
                >
                  <Text style={s.chipText}>{t.enabled ? "disable" : "enable"}</Text>
                </Pressable>
                <Pressable
                  style={s.chip}
                  onLongPress={() =>
                    Alert.alert("delete trigger", `delete "${t.name}"?`, [
                      { text: "cancel", style: "cancel" },
                      {
                        text: "delete",
                        style: "destructive",
                        onPress: () =>
                          relay.send({ t: "TriggerDelete", id: nextId(), client: "mobile", req_id: nextId(), trigger: t.id! } as Frame),
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
        <Text style={s.newBtnText}>new trigger</Text>
      </Pressable>
      {creating && (
        <TriggerCreateForm
          relay={relay}
          workflows={workflowsRef.current}
          onDone={() => setCreating(false)}
        />
      )}
    </View>
  );
}

/// Minimal trigger create form: name, workflow, kind + spec. Editing
/// (and webhook secrets) stay web-side by design; this covers the
/// day-one case "run this workflow every morning".
function TriggerCreateForm({
  relay,
  workflows,
  onDone,
}: {
  relay: Relay;
  workflows: { id: string; name: string }[];
  onDone: () => void;
}) {
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"cron" | "event">("cron");
  const [workflowId, setWorkflowId] = useState(workflows[0]?.id ?? "");
  const [cron, setCron] = useState("0 8 * * *");
  const [event, setEvent] = useState("");
  const [err, setErr] = useState("");

  const save = () => {
    if (!name.trim() || !workflowId) {
      setErr("name and workflow are required");
      return;
    }
    if (kind === "cron" && cron.trim().split(/\s+/).length !== 5) {
      setErr("cron needs 5 fields (m h dom mon dow)");
      return;
    }
    if (kind === "event" && !event.trim()) {
      setErr("event name is required");
      return;
    }
    const rid = nextId();
    relay.send({
      t: "TriggerPut", id: nextId(), client: "mobile", req_id: rid,
      trigger_id: null,
      trigger: {
        name: name.trim(),
        workflow_id: workflowId,
        kind,
        spec: kind === "cron" ? { cron: cron.trim() } : { event: event.trim() },
        enabled: true,
      },
    } as unknown as Frame);
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "TriggerPutOk" && f.req_id === rid) {
        un();
        onDone();
      } else if (f.t === "Error" && f.req_id === rid) {
        un();
        setErr(f.message);
      }
    });
  };

  return (
    <View style={[s.sheet]}>
      <Text style={s.rowTitle}>new trigger</Text>
      <Text style={s.label}>name</Text>
      <TextInput style={s.input} value={name} onChangeText={setName} autoCapitalize="none" placeholder="morning run" placeholderTextColor="#4b5563" />
      <Text style={s.label}>workflow</Text>
      {workflows.length === 0 ? (
        <Text style={s.dim}>no workflows exist — create one first</Text>
      ) : (
        <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 6 }}>
          {workflows.map((w) => (
            <Pressable key={w.id} style={[s.chip, workflowId === w.id && s.chipOn]} onPress={() => setWorkflowId(w.id)}>
              <Text style={[s.chipText, workflowId === w.id && s.chipTextOn]} numberOfLines={1}>{w.name}</Text>
            </Pressable>
          ))}
        </View>
      )}
      <Text style={s.label}>kind</Text>
      <View style={{ flexDirection: "row", gap: 6 }}>
        {(["cron", "event"] as const).map((k) => (
          <Pressable key={k} style={[s.chip, kind === k && s.chipOn]} onPress={() => setKind(k)}>
            <Text style={[s.chipText, kind === k && s.chipTextOn]}>{k}</Text>
          </Pressable>
        ))}
      </View>
      {kind === "cron" ? (
        <>
          <Text style={s.label}>cron (m h dom mon dow)</Text>
          <TextInput style={s.input} value={cron} onChangeText={setCron} autoCapitalize="none" placeholder="0 8 * * *" placeholderTextColor="#4b5563" />
        </>
      ) : (
        <>
          <Text style={s.label}>event name</Text>
          <TextInput style={s.input} value={event} onChangeText={setEvent} autoCapitalize="none" placeholder="ci/build.failed" placeholderTextColor="#4b5563" />
        </>
      )}
      {err !== "" && <Text style={s.errText}>{err}</Text>}
      <View style={{ flexDirection: "row", gap: 8, marginTop: 8 }}>
        <Pressable style={[s.newBtn, { flex: 1 }]} onPress={save}>
          <Text style={s.newBtnText}>create</Text>
        </Pressable>
        <Pressable style={[s.newBtn, { flex: 1, backgroundColor: "#1f2430" }]} onPress={onDone}>
          <Text style={s.newBtnText}>cancel</Text>
        </Pressable>
      </View>
    </View>
  );
}

const s = StyleSheet.create({
  newBtn: {
    backgroundColor: "#16a34a", borderRadius: 8, paddingVertical: 12,
    alignItems: "center", marginVertical: 8,
  },
  newBtnText: { color: "#fff", fontWeight: "700" },
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
  wrap: { flex: 1, backgroundColor: "#101014" },
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
  chipOn: { borderColor: "#4ade80", backgroundColor: "rgba(74,222,128,0.12)" },
  chipTextOn: { color: "#4ade80", fontSize: 13, fontWeight: "600" },
  chipGo: { borderColor: "#4ade80" },
  chipGoText: { color: "#4ade80", fontSize: 13, fontWeight: "700" },
});
