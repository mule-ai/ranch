import { useEffect, useState, useCallback } from "react";
import {
  ActivityIndicator,
  FlatList,
  Pressable,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { supabase } from "../lib/supabase";

type Machine = { id: string; name: string; last_seen_at: string | null };

export function MachinesScreen({ onPick }: { onPick: (m: Machine) => void }) {
  const [machines, setMachines] = useState<Machine[] | null>(null);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    setErr("");
    const { data, error } = await supabase
      .from("machines_info")
      .select("id,name,last_seen_at")
      .order("name");
    if (error) setErr(error.message);
    setMachines(data ?? []);
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  if (machines === null) {
    return (
      <View style={styles.center}>
        <ActivityIndicator color="#4ade80" />
      </View>
    );
  }

  const online = (m: Machine) =>
    m.last_seen_at !== null && Date.now() - Date.parse(m.last_seen_at) < 90_000;

  return (
    <View style={styles.wrap}>
      <Text style={styles.h1}>Machines</Text>
      {err !== "" && <Text style={styles.err}>{err}</Text>}
      <FlatList
        data={machines}
        keyExtractor={(m) => m.id}
        ListEmptyComponent={<Text style={styles.dim}>No machines registered. Run `ranch register` on a host.</Text>}
        renderItem={({ item }) => (
          <Pressable style={styles.row} onPress={() => onPick(item)}>
            <View style={[styles.dot, { backgroundColor: online(item) ? "#4ade80" : "#6b7280" }]} />
            <View style={{ flex: 1 }}>
              <Text style={styles.rowTitle}>{item.name}</Text>
              <Text style={styles.dim}>
                {online(item) ? "online" : `last seen ${item.last_seen_at?.slice(0, 16) ?? "never"}`}
              </Text>
            </View>
            <Text style={styles.chev}>›</Text>
          </Pressable>
        )}
      />
      <Pressable style={styles.btn} onPress={load}>
        <Text style={styles.btnText}>Refresh</Text>
      </Pressable>
      <Pressable
        style={[styles.btn, styles.signOut]}
        onPress={() => supabase.auth.signOut()}
      >
        <Text style={[styles.btnText, { color: "#f87171" }]}>Sign out</Text>
      </Pressable>
    </View>
  );
}

const styles = StyleSheet.create({
  center: { flex: 1, justifyContent: "center", alignItems: "center", backgroundColor: "#101014" },
  wrap: { flex: 1, backgroundColor: "#101014", paddingTop: 60, paddingHorizontal: 16 },
  h1: { color: "#e5e7eb", fontSize: 28, fontWeight: "700", marginBottom: 16 },
  row: {
    flexDirection: "row",
    alignItems: "center",
    gap: 12,
    paddingVertical: 14,
    borderBottomWidth: 1,
    borderBottomColor: "#1f2430",
  },
  rowTitle: { color: "#f3f4f6", fontSize: 17, fontWeight: "600" },
  dot: { width: 10, height: 10, borderRadius: 5 },
  chev: { color: "#4b5563", fontSize: 24 },
  dim: { color: "#6b7280", fontSize: 13 },
  err: { color: "#f87171", marginBottom: 8 },
  btn: {
    backgroundColor: "#1f2937",
    borderRadius: 10,
    paddingVertical: 12,
    alignItems: "center",
    marginTop: 8,
  },
  signOut: { backgroundColor: "transparent", marginBottom: 24 },
  btnText: { color: "#e5e7eb", fontWeight: "600" },
});
