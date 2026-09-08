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
  const [conn, setConn] = useState("connecting…");
  const [history, setHistory] = useState<string[] | null>(null);
  const [blink, setBlink] = useState(true);
  const inputRef = useRef<TextInput | null>(null);
  const lastText = useRef("");
  // predictive local echo: chars sent to the PTY that have not been
  // confirmed by an authoritative Update yet, rendered dimmed at the
  // cursor so typing feels instant despite the relay round trip
  const [pred, setPred] = useState<{ row: number; col: number; text: string } | null>(null);
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
          inputRef.current?.focus();
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
          setPred(null); // authoritative echo arrived
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
    if (!activePane || text === "") return;
    relay.send({
      t: "Input", id: nextId(), client: "mobile",
      session: sessionId, pane: activePane, data: b64(text),
    } as Frame);
  };

  // Raw-mode typing: every keystroke goes straight to the PTY (like an
  // SSH client). The field below is a hidden capture surface — the real
  // echo comes back from the PTY in the pane view. Diffs between the
  // IME's last and current text become input bytes: inserted chars
  // stream through as-is, deletions become backspaces.
  const onType = (text: string) => {
    const prev = lastText.current;
    lastText.current = text;
    let start = 0;
    while (start < prev.length && start < text.length && prev[start] === text[start]) {
      start++;
    }
    let endPrev = prev.length;
    let endNew = text.length;
    while (endPrev > start && endNew > start && prev[endPrev - 1] === text[endNew - 1]) {
      endPrev--;
      endNew--;
    }
    const removed = prev.length - (endPrev - start) - start;
    if (removed > 0) {
      send("\u007f".repeat(removed));
      setPred((pr) => {
        if (!pr || pr.text === "") return null;
        return { ...pr, text: pr.text.slice(0, Math.max(0, pr.text.length - removed)) };
      });
    }
    const added = text.slice(start, endNew);
    if (added !== "") {
      // Enter on a multiline IME shows up as a newline in the diff;
      // newlines can't be predicted (command runs, output streams)
      const printable = added.replace(/[\n\r]/g, "");
      if (printable !== "") {
        setPred((pr) => {
          if (!pr) {
            const c = panesRef.current.get(activePane)?.cursor;
            if (!c) return null;
            return { row: c.y, col: c.x, text: printable };
          }
          return { ...pr, text: pr.text + printable };
        });
      } else {
        setPred(null);
      }
      send(added.replace(/\n/g, "\r"));
    }
  };

  // blinking cursor
  useEffect(() => {
    const t = setInterval(() => setBlink((b) => !b), 530);
    return () => clearInterval(t);
  }, []);

  const loadHistory = () => {
    if (!activePane) return;
    relay.send({
      t: "ScrollbackReq", id: nextId(), client: "mobile",
      session: sessionId, pane: activePane, offset: 0, limit: 2000,
    } as Frame);
  };

  const rects = layout ? layoutRects(layout, 0, 0, COLS, ROWS) : [];

  return (
    <KeyboardAvoidingView
      style={styles.flex}
      behavior="height"
      onTouchStart={() => inputRef.current?.focus()}
    >
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
                inputRef.current?.focus();
                setActivePane(r.pane);
                relay.send({
                  t: "SessionsSelect", session: sessionId, pane: r.pane,
                } as Frame);
              }}
            >
              <ScrollView horizontal={false}>
                <Text style={styles.mono}>
                  {(p?.lines ?? []).slice(0, r.h).map((line, yy) => {
                    const c = p?.cursor;
                    if (focused && c && yy === c.y && pred &&
                        pred.row === c.y && pred.col === c.x && pred.text !== "") {
                      // predicted (unconfirmed) keystrokes: dimmed, with the
                      // cursor riding after them
                      const at = line.charAt(c.x) || " ";
                      return (
                        <Text key={yy}>
                          {line.slice(0, c.x)}
                          <Text style={styles.predText}>{pred.text}</Text>
                          <Text style={styles.cursor}>{at}</Text>
                          {line.slice(c.x + 1)}
                          {"\n"}
                        </Text>
                      );
                    }
                    if (focused && c && c.visible && yy === c.y && (blink || pred)) {
                      // block cursor: reverse-video the cell under it
                      const ch = line.charAt(c.x) || " ";
                      return (
                        <Text key={yy}>
                          {line.slice(0, c.x)}
                          <Text style={styles.cursor}>{ch}</Text>
                          {line.slice(c.x + 1)}
                          {"\n"}
                        </Text>
                      );
                    }
                    return <Text key={yy}>{line + "\n"}</Text>;
                  })}
                </Text>
              </ScrollView>
            </Pressable>
          );
        })}
        {rects.length === 0 && (
          <Text style={styles.dim}>waiting for snapshot…</Text>
        )}
      </View>

      {/* hidden keystroke capture surface — typing goes straight to the
          PTY and the echo renders in the pane above, like SSH */}
      <TextInput
        ref={inputRef}
        style={styles.hiddenInput}
        multiline
        value=""
        onChangeText={onType}
        autoCapitalize="none"
        autoCorrect={false}
        autoComplete="off"
        spellCheck={false}
        blurOnSubmit={false}
        caretHidden
      />

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
  hiddenInput: {
    position: "absolute", opacity: 0.01, height: 1, width: 1,
    left: 0, bottom: 0,
  },
  keys: { flexDirection: "row", gap: 6, paddingHorizontal: 8, paddingBottom: 28 },
  key: { backgroundColor: "#1f2430", borderRadius: 6, paddingVertical: 6, paddingHorizontal: 10 },
  keyText: { color: "#9ca3af", fontSize: 12 },
  cursor: { backgroundColor: "#d1d5db", color: "#101014" },
  predText: { color: "#6b7280" },
  histWrap: {
    position: "absolute", bottom: 0, left: 8, right: 8, top: "18%",
    backgroundColor: "#14151c", borderRadius: 10, padding: 10,
    borderTopWidth: 1, borderTopColor: "#26262e",
  },
  histHeader: { flexDirection: "row", justifyContent: "space-between", marginBottom: 6 },
  histTitle: { color: "#9ca3af", fontSize: 12, fontWeight: "700" },
  histScroll: { flex: 1 },
});
