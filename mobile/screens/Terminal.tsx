import { useEffect, useState, useCallback, useRef } from "react";
import {
  KeyboardAvoidingView,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { Frame, Layout, PaneSnap, b64, nextId } from "../lib/frames";
import { Relay } from "../lib/relay";

// Compute screen rects from the split tree (mirrors the desktop client).
export type Rect = { pane: string; x: number; y: number; w: number; h: number };
export function layoutRects(l: Layout, x: number, y: number, w: number, h: number): Rect[] {
  if (l.k === "Leaf") return [{ pane: l.pane, x, y, w, h }];
  const pct = Math.min(Math.max(l.pct, 0), 100) / 100;
  if (l.dir === 1) {
    const aw = Math.max(1, Math.round(w * pct));
    return [
      ...layoutRects(l.a, x, y, aw, h),
      ...layoutRects(l.b, x + aw, y, w - aw, h),
    ];
  }
  const ah = Math.max(1, Math.round(h * pct));
  return [
    ...layoutRects(l.a, x, y, w, ah),
    ...layoutRects(l.b, x, y + ah, w, h - ah),
  ];
}

type Props = { relay: Relay; sessionId: string; sessionName: string; onExit: () => void };

const COLS = 96;
const ROWS = 30;

export function TerminalScreen({ relay, sessionId, sessionName, onExit }: Props) {
  const [panes, setPanes] = useState<Map<string, PaneSnap>>(new Map());
  const [layout, setLayout] = useState<Layout | null>(null);
  const [activePane, setActivePane] = useState<string>("");
  const [input, setInput] = useState("");
  const [conn, setConn] = useState("connecting…");
  const [history, setHistory] = useState<string[] | null>(null);
  const panesRef = useRef(panes);
  panesRef.current = panes;

  useEffect(() => {
    relay.onFrame = (f: Frame) => {
      switch (f.t) {
        case "HelloOk":
          setConn("online");
          break;
        case "Snapshot": {
          if (f.session !== sessionId) return;
          const m = new Map<string, PaneSnap>();
          for (const p of f.panes) m.set(p.id, p);
          setPanes(m);
          setLayout(f.layout);
          setActivePane(f.active_pane);
          setConn("online");
          break;
        }
        case "Update": {
          if (f.session !== sessionId) return;
          const cur = panesRef.current.get(f.pane);
          if (!cur) return;
          const lines = cur.lines.slice();
          for (const [y, text] of f.rows_upd) {
            if (y < lines.length) lines[y] = text;
          }
          const next = new Map(panesRef.current);
          next.set(f.pane, { ...cur, lines, cursor: f.cursor ?? cur.cursor });
          setPanes(next);
          break;
        }
        case "Scrollback":
          setHistory(f.lines);
          break;
        case "Meta":
          if (f.kind === "exited") setConn("session ended");
          break;
        case "Error":
          setConn(`error: ${f.message}`);
          break;
      }
    };
    relay.onStatus = setConn;
    // hello + attach (resize first so the daemon sizes the PTY for a phone)
    relay.send({ t: "Hello", id: nextId(), client: "mobile" } as Frame);
    relay.send({
      t: "Attach", id: nextId(), client: "mobile", session: sessionId,
    } as Frame);
    relay.send({
      t: "Resize", id: nextId(), client: "mobile", session: sessionId,
      cols: COLS, rows: ROWS,
    } as Frame);
    return () => {
      relay.send({ t: "Detach", id: nextId(), client: "mobile" } as Frame);
      relay.onFrame = () => {};
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  const send = (text: string) => {
    if (!activePane) return;
    relay.send({
      t: "Input", id: nextId(), client: "mobile",
      session: sessionId, pane: activePane, data: b64(text),
    } as Frame);
  };

  const submit = () => {
    send(input + "\n");
    setInput("");
  };

  const loadHistory = () => {
    if (!activePane) return;
    relay.send({
      t: "ScrollbackReq", id: nextId(), client: "mobile",
      session: sessionId, pane: activePane, offset: 0, limit: 2000,
    } as Frame);
  };

  const rects = layout ? layoutRects(layout, 0, 0, COLS, ROWS) : [];

  return (
    <KeyboardAvoidingView style={styles.flex} behavior="height">
      <View style={styles.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={styles.back}>‹ back</Text>
        </Pressable>
        <Text style={styles.title} numberOfLines={1}>{sessionName}</Text>
        <Text style={styles.conn}>{conn}</Text>
      </View>

      <View style={styles.screenWrap}>
        {rects.map((r) => {
          const p = panes.get(r.pane);
          const focused = r.pane === activePane;
          return (
            <Pressable
              key={r.pane}
              style={[
                styles.pane,
                {
                  left: `${(r.x / COLS) * 100}%`,
                  top: `${(r.y / ROWS) * 100}%`,
                  width: `${(r.w / COLS) * 100}%`,
                  height: `${(r.h / ROWS) * 100}%`,
                  borderColor: focused ? "#4ade80" : "#26262e",
                },
              ]}
              onPress={() => {
                setActivePane(r.pane);
                relay.send({
                  t: "SessionsSelect", session: sessionId, pane: r.pane,
                } as Frame);
              }}
            >
              <ScrollView horizontal={false}>
                <Text style={styles.mono}>
                  {(p?.lines ?? []).slice(0, r.h).join("\n")}
                  {p?.cursor?.visible
                    ? "\u2588".repeat(1) // block cursor indicator
                    : ""}
                </Text>
              </ScrollView>
            </Pressable>
          );
        })}
        {rects.length === 0 && (
          <Text style={styles.dim}>waiting for snapshot…</Text>
        )}
      </View>

      <View style={styles.inputRow}>
        <TextInput
          style={[styles.mono, styles.input]}
          value={input}
          onChangeText={setInput}
          onSubmitEditing={submit}
          placeholder="type a command…"
          placeholderTextColor="#4b5563"
          autoCapitalize="none"
          autoCorrect={false}
          autoComplete="off"
          spellCheck={false}
        />
        <Pressable style={styles.sendBtn} onPress={submit}>
          <Text style={styles.btnText}>send</Text>
        </Pressable>
      </View>

      {history !== null ? (
        <View style={styles.histWrap}>
          <View style={styles.histHeader}>
            <Text style={styles.histTitle}>scrollback ({history.length})</Text>
            <Pressable onPress={() => setHistory(null)} hitSlop={8}>
              <Text style={styles.back}>close ✕</Text>
            </Pressable>
          </View>
          <ScrollView style={styles.histScroll}>
            <Text style={styles.mono}>{history.join("\n")}</Text>
          </ScrollView>
        </View>
      ) : (
        <View style={styles.keys}>
          {["Enter", "Ctrl-C", "Ctrl-D", "Ctrl-L", "Tab", "Esc", "↑", "↓", "←", "→", "hist"].map((k) => (
            <Pressable
              key={k}
              style={styles.key}
              onPress={() => {
                if (k === "hist") {
                  loadHistory();
                  return;
                }
                const seq: Record<string, string> = {
                  Enter: "\n", "Ctrl-C": "\x03", "Ctrl-D": "\x04",
                  "Ctrl-L": "\x0c", "Ctrl-R": "\x12", Tab: "\t", Esc: "\x1b",
                  "↑": "\x1b[A", "↓": "\x1b[B", "←": "\x1b[D", "→": "\x1b[C",
                };
                send(seq[k] ?? "");
              }}
            >
              <Text style={styles.keyText}>{k}</Text>
            </Pressable>
          ))}
        </View>
      )}
    </KeyboardAvoidingView>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1, backgroundColor: "#101014" },
  header: {
    flexDirection: "row", alignItems: "center", gap: 12,
    paddingTop: 56, paddingBottom: 8, paddingHorizontal: 12,
  },
  back: { color: "#4ade80", fontSize: 15 },
  title: { color: "#f3f4f6", fontWeight: "700", flex: 1, fontSize: 16 },
  conn: { color: "#6b7280", fontSize: 12 },
  screenWrap: {
    flex: 1, backgroundColor: "#0a0a0e", marginHorizontal: 8,
    borderRadius: 8, overflow: "hidden",
  },
  pane: { position: "absolute", borderWidth: 1, padding: 2 },
  mono: {
    fontFamily: "monospace", fontSize: 9, lineHeight: 12,
    color: "#d1d5db", letterSpacing: -0.2,
  },
  dim: { color: "#6b7280", padding: 16 },
  inputRow: {
    flexDirection: "row", gap: 8, padding: 8, alignItems: "center",
  },
  input: {
    flex: 1, backgroundColor: "#1a1b23", borderRadius: 8,
    paddingHorizontal: 10, paddingVertical: 8, fontSize: 13, color: "#f3f4f6",
  },
  sendBtn: { backgroundColor: "#16a34a", borderRadius: 8, paddingHorizontal: 14, paddingVertical: 10 },
  btnText: { color: "#fff", fontWeight: "700" },
  keys: { flexDirection: "row", gap: 6, paddingHorizontal: 8, paddingBottom: 28 },
  key: { backgroundColor: "#1f2430", borderRadius: 6, paddingVertical: 6, paddingHorizontal: 10 },
  keyText: { color: "#9ca3af", fontSize: 12 },
  cursor: { backgroundColor: "#d1d5db", color: "#101014" },
  histWrap: {
    position: "absolute", bottom: 0, left: 8, right: 8, top: "18%",
    backgroundColor: "#14151c", borderRadius: 10, padding: 10,
    borderTopWidth: 1, borderTopColor: "#26262e",
  },
  histHeader: { flexDirection: "row", justifyContent: "space-between", marginBottom: 6 },
  histTitle: { color: "#9ca3af", fontSize: 12, fontWeight: "700" },
  histScroll: { flex: 1 },
});
