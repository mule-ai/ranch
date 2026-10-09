// Herd H5.3 — the Activity View: the herd's live timeline, mirroring the
// TUI's Activity section. Data is push-only: the daemon's activity
// poller recomputes the full item set (~5 s, on change) and broadcasts
// `ActivityPush`; this screen just replaces its timeline. Items group
// by agent ("local" for machine-wide items), groups ordered by their
// most-recent item, rows "glyph text · HH:MM" (the TUI's H5.3 glyph
// map). Header "pause herd"/"resume herd" button sends
// `AgentPause { agent: null, paused }`; long-press on an item offers
// pause/resume for that item's agent. Tapping an item deep-links to the
// agent (same `AgentTalk{continue}` path as the agents tab).
import { useEffect, useRef, useState } from "react";
import {
  Alert,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { Relay } from "../lib/relay";
import {
  Frame,
  AgentInfo,
  ActivityItem,
  AgentPauseResult,
  nextId,
} from "../lib/frames";

/** One glyph per item kind — the H5.3 glyph map (same as the TUI). */
const GLYPH: Record<string, string> = {
  belief_pending: "🧠",
  ask: "✅",
  research_open: "🔬",
  wake: "⏰",
  forge_task: "▸",
  mule_job: "⇄",
  trigger: "⚡",
  spawn: "✚",
};
const glyph = (kind: string) => GLYPH[kind] ?? "·";

/** "HH:MM" from an RFC 3339 timestamp; "" when the shape doesn't match. */
const hhmm = (ts: string) => (ts.length >= 16 && ts[10] === "T" ? ts.slice(11, 16) : "");

/** One status line for `AgentPauseOk`: ok/failed counts + the first
 *  failure's detail (mirrors the TUI's `activity_pause_status`). */
function pauseStatus(results: AgentPauseResult[]): string {
  if (results.length === 0) return "pause: no agents known";
  const failed = results.filter((r) => r.error).length;
  const base =
    failed === 0
      ? `pause: ${results.length} agent(s) ok`
      : `pause: ${results.length - failed} ok, ${failed} failed`;
  const first = results.find((r) => r.error);
  return first ? `${base} — ${first.name}: ${first.error ?? "unknown error"}` : base;
}

type Props = { relay: Relay; onTalk: (a: AgentInfo) => void };

export function ActivityScreen({ relay, onTalk }: Props) {
  const [items, setItems] = useState<ActivityItem[]>([]);
  const [live, setLive] = useState(false);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [status, setStatus] = useState("");
  // Last CONFIRMED pause target: "herd" or an agent registry name.
  // A failed AgentPauseOk leg leaves it unknown (the toggle targets the
  // pause direction again) — same rule as the TUI.
  const [pausedHint, setPausedHint] = useState<string | null>(null);
  // In-flight pause: which hint to store on a clean AgentPauseOk
  // (null hint = the target was resumed).
  const pendingPause = useRef<{ hint: string | null } | null>(null);

  useEffect(() => {
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "ActivityPush":
          setItems(f.items ?? []);
          setLive(true);
          break;
        case "AgentListOk":
          // deep links resolve item.agent (a registry NAME) -> id
          setAgents(f.agents ?? []);
          break;
        case "AgentPauseOk": {
          const results = f.results ?? [];
          setStatus(pauseStatus(results));
          if (results.every((r) => !r.error) && pendingPause.current) {
            setPausedHint(pendingPause.current.hint);
            pendingPause.current = null;
          }
          break;
        }
        case "Error": {
          const rid = (f as { req_id?: string | null }).req_id ?? "";
          if (rid.startsWith("ap-")) {
            pendingPause.current = null;
            setStatus(`pause: ${f.message}`);
          }
          break;
        }
      }
    });
    // the daemon only pushes ActivityPush on change; the AgentList
    // request feeds deep-link name->id resolution
    relay.send({ t: "AgentList", id: nextId(), client: "mobile", req_id: "al-" + nextId() } as Frame);
    return un;
  }, [relay]);

  const pauseHerd = () => {
    const paused = pausedHint !== "herd";
    pendingPause.current = { hint: paused ? "herd" : null };
    setStatus(paused ? "pausing herd…" : "resuming herd…");
    relay.send({
      t: "AgentPause", id: nextId(), req_id: "ap-" + nextId(), agent: null, paused,
    } as Frame);
  };

  const pauseAgent = (name: string) => {
    const paused = pausedHint !== name;
    pendingPause.current = { hint: paused ? name : null };
    setStatus(paused ? `pausing ${name}…` : `resuming ${name}…`);
    relay.send({
      t: "AgentPause", id: nextId(), req_id: "ap-" + nextId(), agent: name, paused,
    } as Frame);
  };

  const openItem = (item: ActivityItem) => {
    if (item.agent == null) {
      setStatus(
        item.kind === "ask"
          ? "pending approval — open it from the sessions list (v1: no cross-pane focus)"
          : "machine-local event — open it from the sessions list",
      );
      return;
    }
    const a = agents.find((x) => x.name === item.agent);
    if (!a) {
      setStatus(`agent '${item.agent}' not in the registry yet — retry in a moment`);
      return;
    }
    setStatus(`opening ${a.name}…`);
    onTalk(a);
  };

  // groups: one per agent ("local" for machine-wide), ordered by their
  // most-recent item, items newest-first — RFC3339-UTC strings sort
  // lexicographically (the TUI's activity_groups)
  const ordered = items.map((it, i) => ({ it, i })).sort((a, b) => (a.it.ts < b.it.ts ? 1 : a.it.ts > b.it.ts ? -1 : 0));
  const groups: { key: string; idx: number[] }[] = [];
  for (const { i } of ordered) {
    const key = items[i].agent ?? "local";
    const g = groups.find((x) => x.key === key);
    if (g) g.idx.push(i);
    else groups.push({ key, idx: [i] });
  }

  return (
    <View style={s.wrap}>
      <View style={s.header}>
        <Text style={s.herdTitle}>HERD ACTIVITY</Text>
        <Pressable style={[s.chip, s.chipPause]} onPress={pauseHerd} hitSlop={6}>
          <Text style={s.chipPauseText}>
            {pausedHint === "herd" ? "resume herd" : "pause herd"}
          </Text>
        </Pressable>
      </View>
      {live ? (
        <Text style={s.dim}>{items.length} item{items.length === 1 ? "" : "s"} · live</Text>
      ) : null}
      {status ? <Text style={s.err}>{status}</Text> : null}
      <ScrollView contentContainerStyle={{ paddingBottom: 60 }}>
        {items.length === 0 ? (
          <Text style={s.empty}>No activity.</Text>
        ) : (
          groups.map((g) => (
            <View key={g.key}>
              <Text style={s.groupTitle}>{g.key === "local" ? "LOCAL" : g.key.toUpperCase()}</Text>
              {g.idx.map((i) => {
                const it = items[i];
                const time = hhmm(it.ts);
                return (
                  <Pressable
                    key={it.id}
                    style={s.row}
                    onPress={() => openItem(it)}
                    onLongPress={() =>
                      it.agent == null
                        ? Alert.alert("no agent", "machine-local item — nothing to pause")
                        : Alert.alert(it.agent, undefined, [
                            { text: "cancel", style: "cancel" },
                            {
                              text: pausedHint === it.agent ? "resume agent" : "pause agent",
                              onPress: () => pauseAgent(it.agent!),
                            },
                          ])
                    }
                  >
                    <Text style={s.rowText}>
                      {glyph(it.kind)} {it.text}
                      {it.agent != null ? <Text style={s.dim}>  ·  {it.agent}</Text> : null}
                      {time ? <Text style={s.dim}>  {time}</Text> : null}
                    </Text>
                  </Pressable>
                );
              })}
            </View>
          ))
        )}
      </ScrollView>
    </View>
  );
}

const s = StyleSheet.create({
  wrap: { flex: 1, backgroundColor: "#101014" },
  header: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", marginBottom: 8 },
  herdTitle: { color: "#4ade80", fontSize: 12, fontWeight: "700", letterSpacing: 1 },
  dim: { color: "#6b7280", fontSize: 12, marginBottom: 4 },
  err: { color: "#f59e0b", fontSize: 13, marginBottom: 6 },
  empty: { color: "#6b7280", fontSize: 14, marginTop: 20 },
  groupTitle: {
    color: "#4ade80", fontSize: 12, fontWeight: "700", letterSpacing: 1,
    marginTop: 14, marginBottom: 2,
  },
  row: {
    paddingVertical: 10, paddingHorizontal: 4, gap: 2,
    borderBottomWidth: 1, borderBottomColor: "#1f2430",
  },
  rowText: { color: "#d1d5db", fontSize: 14 },
  chip: {
    borderWidth: 1, borderRadius: 999, paddingHorizontal: 14, paddingVertical: 5,
    borderColor: "#374151",
  },
  chipPause: { borderColor: "#f59e0b", backgroundColor: "rgba(245,158,11,0.12)" },
  chipPauseText: { color: "#f59e0b", fontSize: 13, fontWeight: "700" },
});
