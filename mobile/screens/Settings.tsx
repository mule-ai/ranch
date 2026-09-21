// Settings screen: agent-notification toggles + permission status.
// Notifications only fire when the app is in the BACKGROUND (they exist
// to pull the user back in); while the app is the active screen the UI
// already shows every event, so no in-app banner is scheduled.
import { useCallback, useEffect, useState } from "react";
import {
  BackHandler,
  Platform,
  Pressable,
  ScrollView,
  StyleSheet,
  Switch,
  Text,
  View,
} from "react-native";
import * as Notifications from "expo-notifications";
import {
  loadSettings,
  saveSettings,
  testNotification,
  getModuleError,
  getDiagLog,
  getNotifStats,
  type NotifSettings,
} from "../lib/notifications";
import { getAgentFrameStats } from "../lib/notifyEvents";

// Android back gesture/button = the on-screen back control (same pattern
// as Triggers/Workflows).
function useAndroidBack(handler: () => boolean) {
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", handler);
    return () => sub.remove();
  }, [handler]);
}

type Props = { onExit: () => void };

const TOGGLES: { key: keyof NotifSettings; label: string; desc: string }[] = [
  { key: "turn_end", label: "Agent turn finished", desc: "notify when an agent turn ends (working → idle)" },
  { key: "every_message", label: "Every agent message", desc: "notify on each new assistant message" },
  { key: "ignore_tool_calls", label: "Ignore tool calls", desc: "when on, tool calls never notify" },
  { key: "questions", label: "Agent questions", desc: "notify when an agent asks a question" },
];

export function SettingsScreen({ onExit }: Props) {
  const [settings, setSettings] = useState<NotifSettings | null>(null);
  const [perm, setPerm] = useState<{ granted: boolean } | null>(null);
  const [testNote, setTestNote] = useState("");

  const refreshPerm = useCallback(async () => {
    try {
      const p = await Notifications.getPermissionsAsync();
      setPerm({ granted: p.granted });
    } catch {
      setPerm({ granted: false });
    }
  }, []);

  useEffect(() => {
    loadSettings().then(setSettings).catch(() => {});
    refreshPerm();
  }, [refreshPerm]);

  useAndroidBack(useCallback(() => {
    onExit();
    return true;
  }, [onExit]));

  const toggle = (k: keyof NotifSettings, v: boolean) => {
    if (!settings) return;
    const next = { ...settings, [k]: v };
    setSettings(next);
    void saveSettings(next); // save immediately on toggle
  };

  const test = async () => {
    setTestNote("sending…");
    const { ok, detail } = await testNotification();
    setTestNote(ok ? detail : detail);
  };

  const reRequest = async () => {
    try {
      await Notifications.requestPermissionsAsync();
    } catch {
      // ignore
    }
    refreshPerm();
  };

  return (
    <View style={s.wrap}>
      <View style={s.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={s.back}>‹ sessions</Text>
        </Pressable>
        <Text style={s.title}>settings</Text>
        <View style={{ width: 70 }} />
      </View>

      <ScrollView contentContainerStyle={{ paddingBottom: 60 }}>
        <Text style={s.sectionTitle}>notifications</Text>
        <Text style={s.note}>
          Notifications appear when the app is in the background — they're for pulling
          you back in, not for the screen you're already reading.
        </Text>
        {getModuleError() && (
          <Text style={{ color: "#f87171", fontSize: 12, marginBottom: 6 }}>
            ⚠ notifications module unavailable: {getModuleError()}
          </Text>
        )}

        {settings === null ? (
          <Text style={s.dim}>loading…</Text>
        ) : (
          TOGGLES.map((t) => (
            <View key={t.key} style={s.row}>
              <View style={s.rowTextWrap}>
                <Text style={s.rowTitle}>{t.label}</Text>
                <Text style={s.dim}>{t.desc}</Text>
              </View>
              <Switch
                value={settings[t.key]}
                onValueChange={(v) => toggle(t.key, v)}
                trackColor={{ false: "#2a2a34", true: "rgba(74,222,128,0.35)" }}
                thumbColor="#4ade80"
              />
            </View>
          ))
        )}

        <View style={s.row}>
          <View style={s.rowTextWrap}>
            <Text style={s.rowTitle}>Test notification</Text>
            <Text style={s.dim}>sends one right now (only fires when backgrounded)</Text>
          </View>
          <Pressable style={s.testBtn} onPress={test} hitSlop={6}>
            <Text style={s.testBtnText}>test</Text>
          </Pressable>
        </View>
        {testNote !== "" && <Text style={s.testNote}>{testNote}</Text>}

        <View style={s.row}>
          <View style={s.rowTextWrap}>
            <Text style={s.rowTitle}>Notification permission</Text>
            <Text style={s.dim}>
              {perm === null
                ? "checking…"
                : perm.granted
                  ? "granted ✓"
                  : "denied"}
            </Text>
          </View>
          <Pressable
            style={[s.testBtn, !(perm?.granted ?? false) && s.testBtnOn]}
            onPress={reRequest}
            hitSlop={6}
          >
            <Text style={s.testBtnText}>{perm?.granted ? "ok" : "request"}</Text>
          </Pressable>
        </View>
        {perm !== null && !perm.granted && Platform.OS === "android" && (
          <Text style={s.note}>
            Still denied? Allow notifications for Ranch in Android: Settings → Apps →
            Ranch → Notifications.
          </Text>
        )}

        <Text style={s.sectionTitle}>diagnostics</Text>
        <DiagBlock />
      </ScrollView>
    </View>
  );
}

/** Re-renders every 2s so counters stay live while the screen is open. */
function DiagBlock() {
  const [, tick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => tick((x) => x + 1), 2000);
    return () => clearInterval(t);
  }, []);
  const fs = getAgentFrameStats();
  const st = getNotifStats();
  const log = getDiagLog();
  return (
    <View>
      <Text style={s.dim}>
        {`Agent frames received: ${fs.seen}${fs.lastAt ? ` (last ${fs.lastAt})` : " (none yet)"}`}
      </Text>
      <Text style={s.dim}>
        {`fired: turn_end=${st.fired.turn_end} msg=${st.fired.every_message} q=${st.fired.questions}  |  skipped(open)=${st.skippedActive} off=${st.skippedOff} err=${st.errors}`}
      </Text>
      {log.length > 0 && (
        <View style={s.diagBox}>
          {log.slice(-12).map((l, i) => (
            <Text key={i} style={s.diagLine}>{l}</Text>
          ))}
        </View>
      )}
    </View>
  );
}

const s = StyleSheet.create({
  wrap: { flex: 1, backgroundColor: "#101014", paddingHorizontal: 16 },
  header: {
    flexDirection: "row", alignItems: "center", justifyContent: "space-between",
    marginBottom: 12, paddingTop: 4,
  },
  back: { color: "#4ade80", width: 70 },
  title: { color: "#f3f4f6", fontWeight: "700", fontSize: 17 },
  sectionTitle: {
    color: "#4ade80", fontSize: 12, fontWeight: "700", textTransform: "uppercase",
    letterSpacing: 1, marginBottom: 4, marginTop: 8,
  },
  note: { color: "#6b7280", fontSize: 12, marginBottom: 10, lineHeight: 16 },
  row: {
    flexDirection: "row", alignItems: "center", paddingVertical: 12,
    borderBottomWidth: 1, borderBottomColor: "#1f2430", gap: 10,
  },
  rowTextWrap: { flex: 1, gap: 2 },
  rowTitle: { color: "#f3f4f6", fontSize: 15, fontWeight: "600" },
  dim: { color: "#6b7280", fontSize: 12 },
  testBtn: {
    borderWidth: 1, borderColor: "#374151", borderRadius: 8,
    paddingHorizontal: 14, paddingVertical: 6,
  },
  testBtnOn: { borderColor: "#4ade80", backgroundColor: "rgba(74,222,128,0.12)" },
  testBtnText: { color: "#9ca3af", fontSize: 13, fontWeight: "600" },
  testNote: { color: "#f59e0b", fontSize: 12, marginTop: 6, marginBottom: 4 },
  diagBox: {
    backgroundColor: "#1a1b23", borderRadius: 8, padding: 8, marginTop: 8,
  },
  diagLine: { color: "#9ca3af", fontSize: 10, lineHeight: 14 },
});
