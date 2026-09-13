// Triggers screen (Phase D, mobile parity): list cron/event triggers
// with last-run status; run-now + enable/disable. Trigger creation/
// editing lives on the web surface (forms on a phone are the web's job).
import { useCallback, useEffect, useState } from "react";
import {
  ActivityIndicator,
  Alert,
  FlatList,
  Pressable,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { Relay } from "../lib/relay";
import { Frame, TriggerRow, nextId } from "../lib/frames";

type Props = { relay: Relay; onExit: () => void };

export function TriggersScreen({ relay, onExit }: Props) {
  const [triggers, setTriggers] = useState<TriggerRow[] | null>(null);

  const refresh = useCallback(() => {
    relay.send({ t: "TriggerList", id: nextId(), client: "mobile", req_id: nextId() } as Frame);
  }, [relay]);

  useEffect(() => {
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "TriggerListOk":
          setTriggers(f.triggers as unknown as TriggerRow[]);
          break;
        case "TriggerDeleteOk":
        case "TriggerPutOk":
          refresh();
          break;
        case "TriggerFired":
          refresh();
          break;
      }
    });
    refresh();
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
      <View style={s.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={s.back}>‹ sessions</Text>
        </Pressable>
        <Text style={s.title}>triggers</Text>
        <View style={{ width: 70 }} />
      </View>
      {triggers === null ? (
        <ActivityIndicator color="#4ade80" style={{ marginTop: 32 }} />
      ) : (
        <FlatList
          data={triggers}
          keyExtractor={(t) => t.id ?? ""}
          contentContainerStyle={{ paddingBottom: 40 }}
          ListEmptyComponent={
            <Text style={s.dim}>No triggers. Create them on the web app.</Text>
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
