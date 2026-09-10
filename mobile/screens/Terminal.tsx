import { useEffect, useState, useCallback, useRef } from "react";
import {
  Keyboard,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { ChatMsg, Frame, Layout, PaneSnap, b64, nextId } from "../lib/frames";
import { Relay } from "../lib/relay";
import { parseSgrRow, Span as SgrSpan } from "../lib/sgr";

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

const FONT_SIZE = 10; // px; JetBrainsMono advance is 0.6em
const LINE_HEIGHT = 13;

export function TerminalScreen({ relay, sessionId, sessionName, onExit }: Props) {
  const [panes, setPanes] = useState<Map<string, PaneSnap>>(new Map());
  const [layout, setLayout] = useState<Layout | null>(null);
  const [activePane, setActivePane] = useState<string>("");
  const [conn, setConn] = useState("connecting…");
  const [history, setHistory] = useState<string[] | null>(null);
  const [blink, setBlink] = useState(true);
  const inputRef = useRef<TextInput | null>(null);
  const [capture, setCapture] = useState(" ");
  const [geom, setGeom] = useState({ cols: 80, rows: 24 });
  const geomRef = useRef(geom);
  geomRef.current = geom;
  const kbAuto = useRef(false);
  // predictive local echo: chars sent to the PTY that have not been
  // confirmed by an authoritative Update yet, rendered dimmed at the
  // cursor so typing feels instant despite the relay round trip
  const [pred, setPred] = useState<{ row: number; col: number; text: string } | null>(null);
  const predRef = useRef(pred);
  predRef.current = pred;
  // per-pane update sequence: a gap means updates were lost (mobile
  // networks drop WS connections; broadcast has no replay) — re-attach
  // so the daemon re-snapshots
  const lastSeq = useRef<Map<string, number>>(new Map());
  const panesRef = useRef(panes);
  panesRef.current = panes;

  useEffect(() => {
    const unlisten = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "HelloOk":
          setConn("online");
          break;
        case "Snapshot": {
          if (f.session !== sessionId) return;
          gotSnap = true;
          const m = new Map<string, PaneSnap>();
          lastSeq.current.clear();
          for (const p0 of f.panes) {
            // pad to the pane's full row height — the daemon trims
            // trailing blank rows, but row updates address absolute
            // rows and must always land
            const pad = [...p0.lines];
            while (pad.length < p0.rows) pad.push("");
            // heuristic: a trailing user row means the agent is on it
            const chat = p0.chat ?? [];
            const busy = chat[chat.length - 1]?.role === "user";
            m.set(p0.id, { ...p0, lines: pad, agentBusy: busy });
            // seed dedup tracking from the pane's seq at snapshot time:
            // updates at/below this are duplicates or reordered stragglers
            if (p0.seq) lastSeq.current.set(p0.id, p0.seq);
          }
          setPanes(m);
          setLayout(f.layout);
          setActivePane(f.active_pane);
          setConn("online");
          // open the keyboard once on attach — re-snapshots (resize,
          // re-attach) must NOT toggle it or the keyboard thrashes
          if (!kbAuto.current) {
            kbAuto.current = true;
            openKeyboard();
          }
          break;
        }
        case "Update": {
          if (f.session !== sessionId) return;
          const cur = panesRef.current.get(f.pane);
          if (!cur) return;
          const prevSeq = lastSeq.current.get(f.pane) ?? 0;
          if (prevSeq > 0 && f.seq !== prevSeq + 1) {
            if (f.seq <= prevSeq) {
              // duplicate or reordered frame (Realtime broadcast makes no
              // ordering guarantee). Applying it would overwrite fresh
              // rows with stale content. Drop silently.
              break;
            }
            // forward gap: missed updates — a re-attach makes the daemon
            // re-snapshot; stale rows resolve in ~1 RTT
            lastSeq.current.delete(f.pane);
            relay.send({
              t: "Attach", id: nextId(), client: "mobile", session: sessionId,
            } as Frame);
            break;
          }
          lastSeq.current.set(f.pane, f.seq);
          const lines = cur.lines.slice();
          for (const [y, text] of f.rows_upd) {
            while (lines.length <= y) lines.push("");
            lines[y] = text;
          }
          const next = new Map(panesRef.current);
          next.set(f.pane, { ...cur, lines, cursor: f.cursor ?? cur.cursor });
          setPanes(next);
          // drop the prediction once its own row is confirmed by the
          // authoritative echo (other rows changing doesn't invalidate it)
          setPred((pr) =>
            pr && (f.rows_upd ?? []).some(([y]) => y === pr.row) ? null : pr
          );
          break;
        }
        case "Chat": {
          if (f.session !== sessionId) break;
          const cur = panesRef.current.get(f.pane);
          if (!cur) break;
          const chat = f.reset ? [...(f.msgs ?? [])] : [...(cur.chat ?? [])];
          if (!f.reset) {
            for (const m of f.msgs ?? []) {
              const last = chat[chat.length - 1];
              if (!last || m.seq > last.seq) chat.push(m);
            }
          }
          const next = new Map(panesRef.current);
          next.set(f.pane, { ...cur, chat });
          setPanes(next);
          break;
        }
        case "Scrollback":
          setHistory(f.lines);
          break;
        case "Meta":
          if (f.kind === "exited") setConn("session ended");
          if (f.kind === "agent" && f.pane) {
            const cur = panesRef.current.get(f.pane);
            if (cur) {
              const next = new Map(panesRef.current);
              next.set(f.pane, { ...cur, agentBusy: f.status === "working" });
              setPanes(next);
            }
          }
          break;
        case "Error":
          setConn(`error: ${f.message}`);
          break;
      }
    });
    relay.onStatus = setConn;
    // hello + attach + resize, retried every 3s until the first
    // snapshot lands (the daemon may be mid-reconnect)
    let gotSnap = false;
    const poke = () => {
      if (gotSnap) return;
      relay.send({ t: "Hello", id: nextId(), client: "mobile" } as Frame);
      relay.send({
        t: "Attach", id: nextId(), client: "mobile", session: sessionId,
      } as Frame);
      relay.send({
        t: "Resize", id: nextId(), client: "mobile", session: sessionId,
        cols: geomRef.current.cols, rows: geomRef.current.rows,
      } as Frame);
    };
    poke();
    const retryTimer = setInterval(poke, 3000);
    // soft resync: Realtime broadcast can drop or reorder frames; dropped
    // frames leave rows stale forever (no replay). Periodically re-attach
    // so the daemon re-snapshots and any divergence self-heals.
    const resyncTimer = setInterval(() => {
      if (predRef.current) return; // don't disturb in-flight predictions
      relay.send({ t: "Attach", id: nextId(), client: "mobile", session: sessionId } as Frame);
    }, 15000);
    return () => {
      clearInterval(retryTimer);
      clearInterval(resyncTimer);
      relay.send({ t: "Detach", id: nextId(), client: "mobile" } as Frame);
      unlisten();
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
  // SSH client). The capture field holds a single sentinel space; the
  // visible-password keyboard disables IME composition (suggestions /
  // autocorrect), so every key arrives as a discrete change:
  // - text grew past the sentinel  -> the added chars are keystrokes
  // - text is empty (sentinel deleted) -> backspace (DEL)
  // - a newline in the added text  -> Enter (CR)
  // The real echo comes back from the PTY in the pane view.
  const onType = (text: string) => {
    if (text === "") {
      // backspace deleted the sentinel -> DEL
      send("\u007f");
      setPred((pr) => (pr ? { ...pr, text: pr.text.slice(0, -1) } : pr));
      setCapture(" ");
      return;
    }
    if (text === " ") return; // reset, no input
    const added = text.startsWith(" ") ? text.slice(1) : text;
    setCapture(" "); // re-arm the sentinel
    send(added.replace(/\n/g, "\r"));
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
      setPred(null); // Enter/output can't be predicted
    }
  };

  // forge-chat pane draft (the focused pane is a chat pane when set)
  const [chatDraft, setChatDraft] = useState("");
  const chatRef = useRef<TextInput | null>(null);
  const chatScrollRef = useRef<ScrollView | null>(null);

  // blinking cursor
  useEffect(() => {
    const t = setInterval(() => setBlink((b) => !b), 530);
    return () => clearInterval(t);
  }, []);

  // keyboard visibility + height. RN 0.86 runs edge-to-edge on Android,
  // where KeyboardAvoidingView miscomputes — pad the container manually
  // with the measured keyboard height instead. The shorter container
  // re-measures screenWrap, which resizes the PTY so the TUI's bottom
  // line (status bar / input box) rides above the keyboard.
  const [kbOpen, setKbOpen] = useState(false);
  const [kbHeight, setKbHeight] = useState(0);
  useEffect(() => {
    const show = Keyboard.addListener("keyboardDidShow", (e) => {
      setKbOpen(true);
      setKbHeight(e.endCoordinates?.height ?? 0);
    });
    const hide = Keyboard.addListener("keyboardDidHide", () => {
      setKbOpen(false);
      setKbHeight(0);
    });
    return () => {
      show.remove();
      hide.remove();
    };
  }, []);

  // Android keeps TextInput focus even after the keyboard is dismissed,
  // so plain focus() can be a no-op — track real focus state and force a
  // blur/refocus cycle only when reopening after a dismiss
  const [focused, setFocused] = useState(false);
  // gentle: opens the keyboard if unfocused, no-op (no flicker) if focused
  const ensureFocus = () => {
    if (!focused) inputRef.current?.focus();
  };
  const openKeyboard = () => {
    if (focused) return;
    inputRef.current?.blur();
    setTimeout(() => inputRef.current?.focus(), 60);
  };
  const toggleKeyboard = () => {
    if (kbOpen) {
      Keyboard.dismiss();
      inputRef.current?.blur(); // clear the quirk: focus without keyboard
    } else {
      openKeyboard();
    }
  };

  const loadHistory = () => {
    if (!activePane) return;
    relay.send({
      t: "ScrollbackReq", id: nextId(), client: "mobile",
      session: sessionId, pane: activePane, offset: 0, limit: 2000,
    } as Frame);
  };

  const COLS = geom.cols;
  const ROWS = geom.rows;
  const rects = layout ? layoutRects(layout, 0, 0, COLS, ROWS) : [];
  // forge-chat pane UX: when the focused pane is a chat pane the screen
  // becomes a conversation view (bubbles + input) instead of a grid
  const activeSnap = activePane ? panes.get(activePane) : undefined;
  const chatMode = activeSnap?.kind === "forge-chat";
  const chatMsgs = activeSnap?.chat ?? [];

  return (
    <View style={[styles.flex, { paddingBottom: kbHeight }]}>
      <View style={styles.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={styles.back}>‹ back</Text>
        </Pressable>
        <Text style={styles.title} numberOfLines={1}>{sessionName}</Text>
        <Pressable onPress={toggleKeyboard} hitSlop={8}>
          <Text style={styles.kbBtn}>{kbOpen ? "▼ hide" : "▲ kb"}</Text>
        </Pressable>
        <Text style={styles.conn}>{conn}</Text>
      </View>

      {chatMode ? (
        // forge-chat pane: conversation bubbles + input, full area
        <View style={styles.chatWrap}>
          <ScrollView
            contentContainerStyle={styles.chatList}
            onContentSizeChange={(_, h) => chatScrollRef.current?.scrollToEnd({ animated: false })}
            ref={chatScrollRef}
          >
            {chatMsgs
              .filter((m) => !(m.role !== "tool" && !m.text?.trim()))
              .map((m, i) => (
                <ChatBubble key={i} msg={m} />
              ))}
            {chatMsgs.length === 0 && (
              <Text style={styles.dim}>say something to the agent…</Text>
            )}
            {activeSnap?.agentBusy && (
              <View style={[styles.bubble, styles.bubbleAgent]}>
                <Text style={styles.workingText}>● ● ●</Text>
              </View>
            )}
          </ScrollView>
          <View style={styles.chatInputRow}>
            <TextInput
              ref={chatRef}
              style={styles.chatInput}
              value={chatDraft}
              onChangeText={setChatDraft}
              placeholder="message the agent"
              placeholderTextColor="#4b5563"
              multiline
            />
            <Pressable
              style={styles.chatSend}
              onPress={() => {
                const text = chatDraft.trim();
                if (!text || !activePane) return;
                relay.send({
                  t: "ChatSend", id: nextId(), client: "mobile",
                  session: sessionId, pane: activePane, text,
                } as Frame);
                setChatDraft("");
              }}
            >
              <Text style={styles.chatSendText}>send</Text>
            </Pressable>
          </View>
        </View>
      ) : (
      <View
        style={styles.screenWrap}
        onLayout={(e) => {
          const w = e.nativeEvent.layout.width;
          const h = e.nativeEvent.layout.height;
          // height changes here are real: the keyboard shrinks the
          // container (padded above), so resizing the PTY keeps the
          // TUI's bottom line visible above the keyboard
          const cols = Math.max(20, Math.floor(w / (FONT_SIZE * 0.6)) - 1);
          const rows = Math.max(10, Math.floor(h / LINE_HEIGHT) - 1);
          if (cols !== geomRef.current.cols || rows !== geomRef.current.rows) {
            setGeom({ cols, rows });
            // resize the daemon PTY to the device
            relay.send({
              t: "Resize", id: nextId(), client: "mobile", session: sessionId,
              cols, rows,
            } as Frame);
          }
        }}
      >
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
                ensureFocus();
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
                    const spans = parseSgrRow(line);
                    const spanStyle = (sp: SgrSpan): object => ({
                      color: sp.fg,
                      backgroundColor: sp.bg,
                      fontWeight: sp.bold ? "700" : undefined,
                      fontStyle: sp.italic ? "italic" : undefined,
                      textDecorationLine: sp.underline ? "underline" : undefined,
                    });
                    const renderSpans = (spans2: SgrSpan[]) =>
                      spans2.map((sp, si) => (
                        <Text key={si} style={spanStyle(sp)}>
                          {sp.text}
                        </Text>
                      ));
                    if (focused && c && yy === c.y && pred &&
                        pred.row === c.y && pred.col === c.x && pred.text !== "") {
                      // predictive echo: the authoritative row hasn't landed
                      // yet, so splice the dimmed prediction at the cursor
                      // cell — mid-line edits render in place, not at EOL
                      const out: React.ReactNode[] = [];
                      let col = 0;
                      let spliced = false;
                      spans.forEach((sp, si) => {
                        for (let k = 0; k < sp.text.length; k++) {
                          if (!spliced && col === c!.x) {
                            out.push(
                              <Text key={`p-${si}-${k}`} style={styles.predText}>
                                {pred.text}
                              </Text>
                            );
                            out.push(
                              <Text key={`c-${si}-${k}`} style={styles.cursor}>
                                {sp.text[k]}
                              </Text>
                            );
                            spliced = true;
                          } else {
                            out.push(
                              <Text key={`${si}-${k}`} style={spanStyle(sp)}>
                                {sp.text[k]}
                              </Text>
                            );
                          }
                          col++;
                        }
                      });
                      if (!spliced) {
                        // cursor at/past end of line
                        out.push(
                          <Text key="pred" style={styles.predText}>
                            {pred.text}
                          </Text>
                        );
                        out.push(<Text key="cend" style={styles.cursor}> </Text>);
                      }
                      return (
                        <Text key={yy}>
                          {out}
                          {"\n"}
                        </Text>
                      );
                    }
                    if (focused && c && c.visible && yy === c.y && (blink || pred)) {
                      // reverse-video the cell under the cursor: split spans
                      // at the cursor cell boundary. Each char keeps its full
                      // span style (bold/italic/underline included) — dropping
                      // any of them makes the row visibly restyle on blink.
                      const out: React.ReactNode[] = [];
                      let col = 0;
                      let done = false;
                      spans.forEach((sp, si) => {
                        for (let k = 0; k < sp.text.length; k++) {
                          if (col === c!.x) {
                            out.push(
                              <Text key={`${si}-${k}`} style={styles.cursor}>
                                {sp.text[k]}
                              </Text>
                            );
                            done = true;
                          } else {
                            out.push(
                              <Text key={`${si}-${k}`} style={spanStyle(sp)}>
                                {sp.text[k]}
                              </Text>
                            );
                          }
                          col++;
                        }
                      });
                      if (!done) out.push(<Text key="cend" style={styles.cursor}> </Text>);
                      return (
                        <Text key={yy}>
                          {out}
                          {"\n"}
                        </Text>
                      );
                    }
                    return (
                      <Text key={yy}>
                        {renderSpans(spans)}
                        {"\n"}
                      </Text>
                    );
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
      )}

      {/* hidden keystroke capture surface — typing goes straight to the
          PTY and the echo renders in the pane above, like SSH */}
      {!chatMode && (
      <TextInput
        ref={inputRef}
        style={styles.hiddenInput}
        value={capture}
        onFocus={() => setFocused(true)}
        onBlur={() => setFocused(false)}
        onChangeText={onType}
        onSubmitEditing={() => {
          // Enter = run the command
          send("\r");
          setPred(null);
          setCapture(" ");
          requestAnimationFrame(openKeyboard);
        }}
        returnKeyType="send"
        autoCapitalize="none"
        autoCorrect={false}
        autoComplete="off"
        spellCheck={false}
        blurOnSubmit={false}
        caretHidden
      />
      )}

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
      ) : chatMode ? (
        // chat pane: no terminal quick keys
        <View style={{ height: 0 }} />
      ) : (
        <View style={styles.keys}>
          {["←", "↑", "↓", "→", "Enter", "Esc", "Tab", "Ctrl-C", "Ctrl-D", "Ctrl-L", "hist"].map((k) => (
            <Pressable
              key={k}
              style={styles.key}
              onPress={() => {
                if (k === "hist") {
                  loadHistory();
                  ensureFocus();
                  return;
                }
                const seq: Record<string, string> = {
                  Enter: "\r", "Ctrl-C": "\x03", "Ctrl-D": "\x04",
                  "Ctrl-L": "\x0c", "Ctrl-R": "\x12", Tab: "\t", Esc: "\x1b",
                  "↑": "\x1b[A", "↓": "\x1b[B", "←": "\x1b[D", "→": "\x1b[C",
                };
                // predictions only model printable typing — control keys
                // invalidate them (and never let a button tap bounce the
                // keyboard: plain focus is a no-op when already focused)
                setPred(null);
                send(seq[k] ?? "");
                ensureFocus();
              }}
            >
              <Text style={styles.keyText}>{k}</Text>
            </Pressable>
          ))}
        </View>
      )}
    </View>
  );
}

function ChatBubble({ msg }: { msg: ChatMsg }) {
  const isUser = msg.role === "user";
  const isTool = msg.role === "tool";
  // tool rows: collapsed to one line, tap to expand the full output
  const [open, setOpen] = useState(false);
  if (isTool) {
    const label = msg.tool_name || "tool";
    const dur = msg.duration_ms != null ? ` · ${msg.duration_ms}ms` : "";
    return (
      <Pressable
        style={styles.toolRow}
        onPress={() => msg.tool_output && setOpen((o) => !o)}
        disabled={!msg.tool_output}
      >
        <Text style={styles.toolText}>
          ⚙ {label}{dur}{msg.tool_output ? (open ? " ▲" : " ▼") : ""}
        </Text>
        {msg.tool_output ? (
          open ? (
            <ScrollView style={styles.toolOutOpen} nestedScrollEnabled>
              <Text style={styles.toolOutFull}>{msg.tool_output}</Text>
            </ScrollView>
          ) : (
            <Text style={styles.toolOut} numberOfLines={2}>{msg.tool_output}</Text>
          )
        ) : null}
      </Pressable>
    );
  }
  return (
    <View style={[styles.bubble, isUser ? styles.bubbleUser : styles.bubbleAgent]}>
      <Text style={[styles.bubbleText, isUser && { color: "#052e16" }]}>{msg.text}</Text>
    </View>
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
  kbBtn: { color: "#4ade80", fontSize: 13 },
  screenWrap: {
    flex: 1, backgroundColor: "#0a0a0e", marginHorizontal: 8,
    borderRadius: 8, overflow: "hidden",
  },
  pane: { position: "absolute", borderWidth: 1, padding: 2 },
  chatWrap: { flex: 1, backgroundColor: "#0a0a0e", marginHorizontal: 8, borderRadius: 8, overflow: "hidden" },
  chatList: { padding: 10, gap: 8 },
  bubble: {
    maxWidth: "85%", borderRadius: 14, paddingHorizontal: 12,
    paddingVertical: 8, marginVertical: 2,
  },
  bubbleUser: { alignSelf: "flex-end", backgroundColor: "#22c55e" },
  bubbleAgent: { alignSelf: "flex-start", backgroundColor: "#1e1e26", borderWidth: 1, borderColor: "#2c2c36" },
  bubbleText: { color: "#e5e7eb", fontSize: 14, lineHeight: 19 },
  toolRow: {
    alignSelf: "flex-start", backgroundColor: "#14141a", borderRadius: 8,
    paddingHorizontal: 10, paddingVertical: 6, borderWidth: 1,
    borderColor: "#23232c", maxWidth: "90%",
  },
  toolText: { color: "#9ca3af", fontSize: 12, fontFamily: "JetBrainsMono NF Mono" },
  toolOut: { color: "#6b7280", fontSize: 11, fontFamily: "JetBrainsMono NF Mono", marginTop: 4 },
  workingText: { color: "#9ca3af", fontSize: 12, letterSpacing: 3 },
  toolOutOpen: { maxHeight: 220, marginTop: 6 },
  toolOutFull: { color: "#8b8b96", fontSize: 11, fontFamily: "JetBrainsMono NF Mono" },
  chatInputRow: {
    flexDirection: "row", borderTopWidth: 1, borderTopColor: "#23232c",
    padding: 8, paddingTop: 10, paddingBottom: 30, gap: 8,
    alignItems: "flex-end",
  },
  chatInput: {
    flex: 1, backgroundColor: "#16161c", borderRadius: 10, color: "#f3f4f6",
    paddingHorizontal: 12, paddingVertical: 8, fontSize: 14,
    maxHeight: 100,
  },
  chatSend: { backgroundColor: "#a855f7", borderRadius: 10, paddingHorizontal: 14, paddingVertical: 10, justifyContent: "center" },
  chatSendText: { color: "#fff", fontWeight: "700", fontSize: 13 },
  mono: {
    fontFamily: "JetBrainsMono NF Mono", fontSize: FONT_SIZE,
    lineHeight: LINE_HEIGHT, color: "#d1d5db",
  },
  dim: { color: "#6b7280", padding: 16 },
  hiddenInput: {
    position: "absolute", opacity: 0.01, height: 1, width: 1,
    left: 0, bottom: 0,
  },
  keys: {
    flexDirection: "row", flexWrap: "wrap", gap: 6,
    paddingHorizontal: 8, paddingBottom: 28,
  },
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
