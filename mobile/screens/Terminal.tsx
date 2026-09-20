import { useEffect, useState, useCallback, useRef } from "react";
import {
  ActivityIndicator,
  Alert,
  BackHandler,
  Keyboard,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import * as Clipboard from "expo-clipboard";
import * as DocumentPicker from "expo-document-picker";
import { ChatMsg, Frame, Layout, ModelChoice, PaneSnap, b64, nextId } from "../lib/frames";
import { Relay } from "../lib/relay";
import { parseSgrRow, Span as SgrSpan } from "../lib/sgr";
import {
  getCachedChat,
  mergeChat,
  saveCachedChat,
} from "../lib/chatCache";

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
// how many chat rows to load on attach; older rows page in on scrollback
const CHAT_TAIL = 25;
const CHAT_PAGE = 50;

export function TerminalScreen({ relay, sessionId, sessionName, onExit }: Props) {
  const [panes, setPanes] = useState<Map<string, PaneSnap>>(new Map());
  const [layout, setLayout] = useState<Layout | null>(null);
  const [activePane, setActivePane] = useState<string>("");
  const [conn, setConn] = useState("connecting…");
  // back gesture/button = the on-screen back control (detach to sessions)
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", () => {
      onExit();
      return true;
    });
    return () => sub.remove();
  }, [onExit]);
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
  // chat scrollback pagination (chat panes): pending ChatHistory request,
  // scroll position + content height captured at request time so the
  // view doesn't jump when older rows are prepended
  const chatHistReq = useRef<string | null>(null);
  const [chatLoadingOlder, setChatLoadingOlder] = useState(false);
  const chatScrollAnchor = useRef<{ y: number; contentH: number } | null>(null);
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
            // chat panes: the snapshot only carries the last CHAT_TAIL
            // rows (Attach.chat_limit) — merge in any older rows we
            // already paged in so scrollback survives re-open
            let chat = p0.chat ?? [];
            let chatHasMore = !!p0.chat_has_more;
            if (p0.kind === "forge-chat") {
              const merged = mergeChat(getCachedChat(sessionId, p0.id), chat, chatHasMore);
              chat = merged.msgs;
              chatHasMore = merged.hasMore;
              saveCachedChat(sessionId, p0.id, chat, chatHasMore);
            }
            // heuristic: a trailing user row means the agent is on it
            const busy = chat[chat.length - 1]?.role === "user";
            m.set(p0.id, { ...p0, lines: pad, agentBusy: busy, chat: p0.kind === "forge-chat" ? chat : p0.chat, chat_has_more: chatHasMore ? true : undefined });
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
              chat_limit: CHAT_TAIL,
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
          // keep the on-device cache in step: a reset (e.g. post-compact
          // history) wipes it, appends extend it
          const cached = getCachedChat(sessionId, f.pane) ?? { msgs: [], hasMore: false };
          saveCachedChat(sessionId, f.pane, f.reset ? chat : [...cached.msgs, ...chat.filter((m) => !cached.msgs.some((c) => c.seq === m.seq))], f.reset ? false : cached.hasMore);
          break;
        }
        case "ChatHistoryOk": {
          // older chat rows paged in via scrollback — only act on OUR
          // request, only if the pane still exists
          if (chatHistReq.current !== f.req_id) break;
          chatHistReq.current = null;
          setChatLoadingOlder(false);
          const anchor = chatScrollAnchor.current;
          chatScrollAnchor.current = null;
          const cur = panesRef.current.get(f.pane);
          if (!cur || f.msgs.length === 0) break;
          const existing = new Set((cur.chat ?? []).map((m) => m.seq));
          const add = f.msgs.filter((m) => !existing.has(m.seq));
          if (add.length === 0) break;
          const chat = [...add, ...(cur.chat ?? [])];
          const next = new Map(panesRef.current);
          next.set(f.pane, { ...cur, chat, chat_has_more: f.has_more });
          setPanes(next);
          saveCachedChat(sessionId, f.pane, chat, f.has_more);
          // hold the view in place: shift down by the content that was
          // just prepended (fires from onContentSizeChange once the
          // layout settles)
          if (anchor) chatScrollAnchor.current = anchor;
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
          if (f.kind === "model" && f.pane && f.status) {
            // the pane's active model changed / was reported
            const cur = panesRef.current.get(f.pane);
            if (cur) {
              const next = new Map(panesRef.current);
              next.set(f.pane, { ...cur, model: f.status });
              setPanes(next);
            }
            modelSetReq.current = null; // the switch was confirmed
          }
          if (f.kind === "context" && f.pane && f.status) {
            // the pane's context-window usage readout
            const cur = panesRef.current.get(f.pane);
            if (cur) {
              const next = new Map(panesRef.current);
              next.set(f.pane, { ...cur, context: f.status });
              setPanes(next);
            }
          }
          break;
        case "ModelListOk": {
          setModelOpts((o) => ({ ...o, [f.pane]: f.models }));
          if (f.current) {
            const cur = panesRef.current.get(f.pane);
            if (cur) {
              const next = new Map(panesRef.current);
              next.set(f.pane, { ...cur, model: f.current.name });
              setPanes(next);
            }
          }
          break;
        }
        case "Error":
          // request-scoped errors (model switch, file upload, etc.) only
          // matter when the req_id is one we sent
          if (f.req_id && f.req_id !== modelSetReq.current && f.req_id !== putReqRef.current) break;
          modelSetReq.current = null;
          putReqRef.current = null;
          setConn(`error: ${f.message}`);
          break;
        case "DirListOk": {
          if (f.req_id === attachDirReqRef.current) {
            attachDirReqRef.current = null;
            setAttachBrowse({ path: f.path, parent: f.parent ?? null, dirs: f.dirs, files: f.files ?? [] });
          }
          break;
        }
        case "FilePutOk": {
          // device-file upload landed on the daemon; attach its path
          if (f.req_id === putReqRef.current) {
            putReqRef.current = null;
            setChatAttachments((prev) =>
              prev.includes(f.path) ? prev : [...prev, f.path]
            );
          }
          break;
        }
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
        chat_limit: CHAT_TAIL,
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
      relay.send({ t: "Attach", id: nextId(), client: "mobile", session: sessionId, chat_limit: CHAT_TAIL } as Frame);
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
  // attachments: file paths selected for the next chat message
  const [chatAttachments, setChatAttachments] = useState<string[]>([]);
  // file browser state for the attachment picker
  const [attachBrowse, setAttachBrowse] = useState<{ path: string; parent: string | null; dirs: string[]; files: string[] } | null>(null);
  const attachDirReqRef = useRef<string | null>(null);
  // req_id of an in-flight device-file upload (FilePut -> FilePutOk)
  const putReqRef = useRef<string | null>(null);
  // upload a file picked from the phone to the daemon's uploads dir,
  // then attach the returned path to the next chat message
  const pickFromDevice = useCallback(async () => {
    try {
      const res = await DocumentPicker.getDocumentAsync({ base64: true });
      if (res.canceled || !res.assets?.length) return;
      const a = res.assets[0];
      if (!a.base64) {
        setConn("could not read that file");
        return;
      }
      if (a.size && a.size > 10 * 1024 * 1024) {
        Alert.alert("file too large", "uploads are capped at 10 MB");
        return;
      }
      setConn("uploading…");
      const rid = nextId();
      putReqRef.current = rid;
      relay.send({ t: "FilePut", id: nextId(), client: "mobile", req_id: rid, name: a.name, b64: a.base64 } as Frame);
    } catch (e) {
      putReqRef.current = null;
      setConn(`upload failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  }, [relay, setConn]);
  // agent model picker (chat panes): catalog per pane + open/closed.
  // The pane's active model lives on the PaneSnap (`model`).
  const [modelOpts, setModelOpts] = useState<Record<string, ModelChoice[]>>({});
  const [modelPicker, setModelPicker] = useState(false);
  // free-text filter for the model picker (catalogs are long; scroll-
  // hunting for e.g. "sonnet" is miserable on a phone)
  const [modelQuery, setModelQuery] = useState("");
  // req_id of the last ModelSet we sent (error correlation)
  const modelSetReq = useRef<string | null>(null);
  const chatRef = useRef<TextInput | null>(null);
  const chatScrollRef = useRef<ScrollView | null>(null);
  // sticky-bottom chat: only auto-follow when the user is at the bottom;
  // leave them where they are when they've scrolled up
  const chatAtBottomRef = useRef(true);

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

  // chat scrollback: page in older rows when the user nears the top.
  // Captures the scroll position so the view doesn't jump when the
  // older rows are prepended (see onContentSizeChange).
  const loadOlder = (offY: number, contentH: number) => {
    if (!activePane || chatHistReq.current) return;
    const snap = panesRef.current.get(activePane);
    const msgs = snap?.chat ?? [];
    if (msgs.length === 0 || !snap?.chat_has_more) return;
    const rid = nextId();
    chatHistReq.current = rid;
    setChatLoadingOlder(true);
    chatScrollAnchor.current = { y: offY, contentH };
    relay.send({
      t: "ChatHistory", id: nextId(), client: "mobile",
      session: sessionId, pane: activePane, req_id: rid,
      limit: CHAT_PAGE, before: msgs[0].seq,
    } as Frame);
  };

  // agent model picker: open the overlay; fetch the catalog from the
  // daemon on first use (ModelListOk lands into modelOpts[pane])
  const openModelPicker = () => {
    if (!activePane) return;
    setModelQuery("");
    setModelPicker(true);
    const opts = modelOpts[activePane];
    if (!opts || opts.length === 0) {
      relay.send({
        t: "ModelList", id: nextId(), client: "mobile",
        pane: activePane, req_id: "model-list",
      } as Frame);
    }
  };
  const pickModel = (m: ModelChoice) => {
    if (!activePane) return;
    const rid = `model-set-${Date.now()}`;
    modelSetReq.current = rid;
    relay.send({
      t: "ModelSet", id: nextId(), client: "mobile",
      session: sessionId, pane: activePane,
      provider: m.provider, model: m.id, req_id: rid,
    } as Frame);
    setModelPicker(false);
  };

  const COLS = geom.cols;
  const ROWS = geom.rows;
  const rects = layout ? layoutRects(layout, 0, 0, COLS, ROWS) : [];
  // forge-chat pane UX: when the focused pane is a chat pane the screen
  // becomes a conversation view (bubbles + input) instead of a grid
  const activeSnap = activePane ? panes.get(activePane) : undefined;
  const chatMode = activeSnap?.kind === "forge-chat";
  const chatMsgs = activeSnap?.chat ?? [];
  // model picker: the focused pane's catalog, filtered by the search
  // box (empty query = everything). Computed here so the sheet can
  // render rows AND the empty-hint without one eating the other.
  const paneModels = (activePane ? modelOpts[activePane] : undefined) ?? [];
  const modelQuery_ = modelQuery.trim().toLowerCase();
  const filteredModels = modelQuery_
    ? paneModels.filter(
        (m) =>
          m.name.toLowerCase().includes(modelQuery_) ||
          m.id.toLowerCase().includes(modelQuery_) ||
          m.provider.toLowerCase().includes(modelQuery_),
      )
    : paneModels;
  // switching panes/sessions re-arms bottom-follow so the new
  // conversation opens at the newest message
  useEffect(() => {
    chatAtBottomRef.current = true;
  }, [activePane, sessionId]);

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

      {chatMode && (
        // active-model chip: tap to open the model picker
        <View style={styles.modelBar}>
          <Pressable style={styles.modelChip} onPress={openModelPicker} hitSlop={8}>
            <Text
              style={styles.modelChipText}
              numberOfLines={1}
              adjustsFontSizeToFit
              minimumFontScale={0.8}
            >
              ◈ {activeSnap?.model ?? "pick a model…"}
            </Text>
          </Pressable>
          <Text style={styles.modelHint}>tap to switch</Text>
          {activeSnap?.context ? (
            <Pressable
              style={[styles.modelChip, styles.ctxChip]}
              hitSlop={8}
              onPress={() => {
                if (!activePane) return;
                // compaction summarizes/drops older turns — confirm first
                Alert.alert(
                  "Compact context?",
                  "Compaction summarizes the conversation so far and " +
                    "frees context window. Older message detail is dropped.",
                  [
                    { text: "Cancel", style: "cancel" },
                    {
                      text: "Compact",
                      onPress: () => {
                        relay.send({
                          t: "ChatCompact", id: nextId(), client: "mobile",
                          session: sessionId, pane: activePane, req_id: `compact-${Date.now()}`,
                        } as Frame);
                      },
                    },
                  ],
                );
              }}
            >
              <Text
                style={styles.ctxChipText}
                numberOfLines={1}
                adjustsFontSizeToFit
                minimumFontScale={0.8}
              >
                {activeSnap.context} · tap to compact
              </Text>
            </Pressable>
          ) : null}
        </View>
      )}

      {chatMode ? (
        // forge-chat pane: conversation bubbles + input, full area
        <View style={styles.chatWrap}>
          <ScrollView
            contentContainerStyle={styles.chatList}
            onContentSizeChange={(_, h) => {
              const anchor = chatScrollAnchor.current;
              if (anchor) {
                // a scrollback page was just prepended: shift the view
                // down by the new content height so the user stays put
                chatScrollAnchor.current = null;
                const dy = h - anchor.contentH;
                if (dy > 0)
                  chatScrollRef.current?.scrollTo({ x: 0, y: anchor.y + dy, animated: false });
                return;
              }
              if (chatAtBottomRef.current)
                chatScrollRef.current?.scrollToEnd({ animated: false });
            }}
            onScroll={(e) => {
              const { contentOffset, contentSize, layoutMeasurement } = e.nativeEvent;
              const atBottom =
                contentSize.height - contentOffset.y - layoutMeasurement.height < 24;
              chatAtBottomRef.current = atBottom;
              if (!atBottom && contentOffset.y < 60) loadOlder(contentOffset.y, contentSize.height);
            }}
            scrollEventThrottle={16}
            ref={chatScrollRef}
          >
            {chatLoadingOlder && (
              <View style={{ flexDirection: "row", justifyContent: "center", paddingVertical: 4 }}>
                <ActivityIndicator size="small" color="#6b7280" />
              </View>
            )}
            {!chatLoadingOlder && !activeSnap?.chat_has_more && chatMsgs.length > 0 && (
              <Text style={{ ...styles.dim, textAlign: "center", fontSize: 11 }}>
                — beginning of conversation —
              </Text>
            )}
            {chatMsgs
              .filter((m) => !(m.role !== "tool" && !m.text?.trim()))
              .map((m, i) => (
                <ChatBubble key={m.seq ?? i} msg={m} />
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
            {chatAttachments.length > 0 && (
              <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 4, paddingHorizontal: 8, paddingVertical: 4 }}>
                {chatAttachments.map((a) => (
                  <Pressable key={a} onPress={() => setChatAttachments(prev => prev.filter(p => p !== a))} style={{ backgroundColor: 'rgba(0,0,0,0.3)', borderRadius: 12, paddingHorizontal: 8, paddingVertical: 3 }}>
                    <Text style={{ color: '#8f8', fontSize: 11 }}>📎 {a.split('/').pop()} ✕</Text>
                  </Pressable>
                ))}
              </View>
            )}
            <View style={{ flexDirection: 'row', alignItems: 'center', gap: 6 }}>
              <Pressable
                style={{ paddingHorizontal: 10, paddingVertical: 6, backgroundColor: '#1e1e26', borderRadius: 6 }}
                onPress={() => {
                  Alert.alert("attach a file", undefined, [
                    {
                      text: "📱 pick from device",
                      onPress: pickFromDevice,
                    },
                    {
                      text: "🖥️ browse daemon files",
                      onPress: () => {
                        setAttachBrowse(null);
                        const rid = nextId();
                        attachDirReqRef.current = rid;
                        relay.send({ t: "DirList", id: nextId(), client: "mobile", req_id: rid } as Frame);
                      },
                    },
                    { text: "cancel", style: "cancel" },
                  ]);
                }}
              >
                <Text style={{ color: '#888', fontSize: 16 }}>📎</Text>
              </Pressable>
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
                  const text = chatDraft.trim() || (chatAttachments.length > 0 ? "(see attached files)" : "");
                  if (!text || !activePane) return;
                  chatAtBottomRef.current = true;
                  relay.send({
                    t: "ChatSend", id: nextId(), client: "mobile",
                    session: sessionId, pane: activePane, text,
                    attachments: chatAttachments.length > 0 ? chatAttachments : undefined,
                  } as Frame);
                  setChatDraft("");
                  setChatAttachments([]);
                }}
              >
                <Text style={styles.chatSendText}>send</Text>
              </Pressable>
            </View>
          </View>
          {attachBrowse !== null && chatMode && (
            <View style={{ position: 'absolute', bottom: 56, left: 8, right: 8, maxHeight: 250, backgroundColor: '#1a1a2e', borderRadius: 8, borderWidth: 1, borderColor: '#444', padding: 8, zIndex: 100 }}>
              <View style={{ flexDirection: 'row', justifyContent: 'space-between', marginBottom: 6 }}>
                <Text style={{ color: '#888', fontSize: 12 }}>{attachBrowse.path}</Text>
                <View style={{ flexDirection: 'row', gap: 12 }}>
                  {attachBrowse.parent ? (
                    <Pressable onPress={() => {
                      const rid = nextId();
                      attachDirReqRef.current = rid;
                      relay.send({ t: "DirList", id: nextId(), client: "mobile", req_id: rid, path: attachBrowse.parent } as Frame);
                    }}>
                      <Text style={{ color: '#88f' }}>← up</Text>
                    </Pressable>
                  ) : null}
                  <Pressable onPress={() => setAttachBrowse(null)}>
                    <Text style={{ color: '#f66' }}>✕ close</Text>
                  </Pressable>
                </View>
              </View>
              <ScrollView style={{ maxHeight: 180 }}>
                {attachBrowse.dirs.map((d) => (
                  <Pressable key={d} onPress={() => {
                    const rid = nextId();
                    attachDirReqRef.current = rid;
                    relay.send({ t: "DirList", id: nextId(), client: "mobile", req_id: rid, path: `${attachBrowse.path}/${d}` } as Frame);
                  }} style={{ paddingVertical: 4 }}>
                    <Text style={{ color: '#ccc', fontSize: 13 }}>📁 {d}</Text>
                  </Pressable>
                ))}
                {attachBrowse.files.map((f) => (
                  <Pressable key={f} onPress={() => setChatAttachments(prev => prev.includes(`${attachBrowse.path}/${f}`) ? prev : [...prev, `${attachBrowse.path}/${f}`])} style={{ paddingVertical: 4 }}>
                    <Text style={{ color: '#8f8', fontSize: 13 }}>📄 {f}</Text>
                  </Pressable>
                ))}
              </ScrollView>
            </View>
          )}
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
          <View style={{ alignItems: "center", marginTop: 32, gap: 10 }}>
            <ActivityIndicator color="#4ade80" />
            <Text style={styles.dim}>waiting for snapshot… ({conn})</Text>
          </View>
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

      {modelPicker && chatMode ? (
        <View style={styles.pickerOverlay}>
          <View style={styles.picker}>
            <View style={styles.pickerHeader}>
              <Text style={styles.pickerTitle}>switch agent model</Text>
              <Pressable onPress={() => setModelPicker(false)} hitSlop={8}>
                <Text style={styles.back}>close ✕</Text>
              </Pressable>
            </View>
            <TextInput
              style={styles.pickerSearch}
              value={modelQuery}
              onChangeText={setModelQuery}
              placeholder="filter models…"
              placeholderTextColor="#4b5563"
              autoCapitalize="none"
              autoCorrect={false}
              autoComplete="off"
              spellCheck={false}
            />
            <ScrollView style={styles.pickerList} keyboardShouldPersistTaps="handled">
              {paneModels.length === 0 ? (
                <Text style={styles.dim}>loading models…</Text>
              ) : (
                <>
                  {filteredModels.map((m) => {
                    const active = activeSnap?.model === m.name;
                    return (
                      <Pressable
                        key={`${m.provider}/${m.id}`}
                        style={[styles.pickerRow, active && styles.pickerRowActive]}
                        onPress={() => pickModel(m)}
                      >
                        <Text style={[styles.pickerRowText, active && { color: "#4ade80" }]} numberOfLines={1}>
                          {active ? "◈ " : "  "}{m.name}
                          {m.provider ? ` · ${m.provider}` : ""}
                        </Text>
                      </Pressable>
                    );
                  })}
                  {filteredModels.length === 0 && (
                    <Text style={styles.dim}>no models match "{modelQuery.trim()}"</Text>
                  )}
                </>
              )}
            </ScrollView>
          </View>
        </View>
      ) : null}

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

// --- Helpers ---
function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60000) {
    const s = ms / 1000;
    return s < 10 ? `${s.toFixed(1)}s` : `${Math.round(s)}s`;
  }
  if (ms < 3600000) {
    const m = Math.floor(ms / 60000);
    const s = Math.floor((ms % 60000) / 1000);
    return s > 0 ? `${m}m ${s}s` : `${m}m`;
  }
  const h = Math.floor(ms / 3600000);
  const m = Math.floor((ms % 3600000) / 60000);
  return m > 0 ? `${h}h ${m}m` : `${h}h`;
}

function localTime(iso: string | null | undefined): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (isNaN(d.getTime())) return null;
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

// Simple markdown renderer: handles headers, bold, italic, inline code,
// fenced code blocks, and list items. Returns an array of styled <Text>/fragments.
function renderMarkdown(text: string, baseStyle: any) {
  const elements: any[] = [];
  let codeBlock = false;
  const lines = text.split("\n");
  let codeBuffer: string[] = [];

  // Inline formatter for a single line
  function inline(line: string, keyPrefix: string): any[] {
    const parts: any[] = [];
    let remaining = line;
    let k = 0;
    while (remaining.length > 0) {
      // Check for inline code
      const codeIdx = remaining.indexOf("`");
      // Check for bold
      const boldIdx = remaining.indexOf("**");
      // Check for italic (single *)
      let italicIdx = -1;
      let searchFrom = 0;
      while (searchFrom < remaining.length) {
        const idx = remaining.indexOf("*", searchFrom);
        if (idx === -1) break;
        // skip if it's part of **
        if (remaining[idx + 1] === "*") { searchFrom = idx + 2; continue; }
        if (remaining[idx - 1] === "*") { searchFrom = idx + 1; continue; }
        italicIdx = idx;
        break;
      }

      // Find the earliest special token
      let earliest = -1;
      let earliestType = "";
      if (codeIdx !== -1 && (earliest === -1 || codeIdx < earliest)) { earliest = codeIdx; earliestType = "code"; }
      if (boldIdx !== -1 && (earliest === -1 || boldIdx < earliest)) { earliest = boldIdx; earliestType = "bold"; }
      if (italicIdx !== -1 && (earliest === -1 || italicIdx < earliest)) { earliest = italicIdx; earliestType = "italic"; }

      if (earliest === -1) {
        parts.push(<Text key={`${keyPrefix}-${k++}`} style={baseStyle}>{remaining}</Text>);
        break;
      }

      if (earliest > 0) {
        parts.push(<Text key={`${keyPrefix}-${k++}`} style={baseStyle}>{remaining.slice(0, earliest)}</Text>);
      }

      if (earliestType === "code") {
        const endIdx = remaining.indexOf("`", earliest + 1);
        if (endIdx !== -1) {
          const code = remaining.slice(earliest + 1, endIdx);
          parts.push(<Text key={`${keyPrefix}-${k++}`} style={[baseStyle, { fontFamily: "monospace", backgroundColor: "#1e1e26", color: "#e879f9", paddingHorizontal: 3 }]}>{code}</Text>);
          remaining = remaining.slice(endIdx + 1);
        } else {
          parts.push(<Text key={`${keyPrefix}-${k++}`} style={baseStyle}>{remaining}</Text>);
          break;
        }
      } else if (earliestType === "bold") {
        const endIdx = remaining.indexOf("**", earliest + 2);
        if (endIdx !== -1) {
          const bold = remaining.slice(earliest + 2, endIdx);
          parts.push(<Text key={`${keyPrefix}-${k++}`} style={[baseStyle, { fontWeight: "700" }]}>{bold}</Text>);
          remaining = remaining.slice(endIdx + 2);
        } else {
          parts.push(<Text key={`${keyPrefix}-${k++}`} style={baseStyle}>{remaining}</Text>);
          break;
        }
      } else { // italic
        const endIdx = remaining.indexOf("*", earliest + 1);
        if (endIdx !== -1 && remaining[earliest + 1] !== "*") {
          const italic = remaining.slice(earliest + 1, endIdx);
          parts.push(<Text key={`${keyPrefix}-${k++}`} style={[baseStyle, { fontStyle: "italic" }]}>{italic}</Text>);
          remaining = remaining.slice(endIdx + 1);
        } else {
          parts.push(<Text key={`${keyPrefix}-${k++}`} style={baseStyle}>{remaining}</Text>);
          break;
        }
      }
    }
    return parts;
  }

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    // Handle fenced code blocks
    const fenceMatch = line.match(/^```(\w*)/);
    if (fenceMatch) {
      if (!codeBlock) {
        codeBlock = true;
        codeBuffer = [];
      } else {
        codeBlock = false;
        elements.push(
          <Text key={`code-${i}`} style={[baseStyle, { fontFamily: "monospace", backgroundColor: "#14141a", color: "#a5b4fc", paddingHorizontal: 6, paddingVertical: 4, borderRadius: 4 }]}>
            {codeBuffer.join("\n")}
          </Text>
        );
        codeBuffer = [];
      }
      continue;
    }
    if (codeBlock) {
      codeBuffer.push(line);
      continue;
    }

    // Headers
    const headerMatch = line.match(/^(#{1,6})\s+(.*)/);
    if (headerMatch) {
      const level = headerMatch[1].length;
      const fontSize = level <= 1 ? 18 : level === 2 ? 16 : level === 3 ? 15 : 14;
      elements.push(
        <Text key={`h-${i}`} style={[baseStyle, { fontWeight: "700", fontSize, marginBottom: 2 }]}>
          {inline(headerMatch[2], `h${i}`)}
        </Text>
      );
      continue;
    }

    // List items
    const listMatch = line.match(/^\s*(-|\*|\d+\.)\s+(.*)/);
    if (listMatch) {
      const bullet = listMatch[1] === "*" || listMatch[1] === "-" ? "•" : `${listMatch[1].replace(".", "")} `;
      elements.push(
        <Text key={`li-${i}`} style={[baseStyle, { paddingLeft: 12 }]}>
          {`${bullet} `}
          {inline(listMatch[2], `li${i}`)}
        </Text>
      );
      continue;
    }

    // Regular line
    elements.push(
      <Text key={`p-${i}`} style={baseStyle}>
        {inline(line, `p${i}`)}
      </Text>
    );
  }

  // Flush any unclosed code block
  if (codeBlock && codeBuffer.length > 0) {
    elements.push(
      <Text key={`code-open`} style={[baseStyle, { fontFamily: "monospace", backgroundColor: "#14141a", color: "#a5b4fc", paddingHorizontal: 6, paddingVertical: 4, borderRadius: 4 }]}>
        {codeBuffer.join("\n")}
      </Text>
    );
  }

  return elements;
}

function ChatBubble({ msg }: { msg: ChatMsg }) {
  const isUser = msg.role === "user";
  const isTool = msg.role === "tool";
  const ts = localTime(msg.created_at);
  // tool rows: collapsed to one line, tap to expand the full output
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const handleCopy = async () => {
    const text = isTool ? (msg.tool_output || msg.tool_name || "") : msg.text;
    if (!text.trim()) return;
    await Clipboard.setStringAsync(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };
  if (isTool) {
    const label = msg.tool_name || "tool";
    const dur = msg.duration_ms != null ? ` · ${formatDuration(msg.duration_ms)}` : "";
    return (
      <Pressable
        style={styles.toolRow}
        onPress={() => msg.tool_output && setOpen((o) => !o)}
        onLongPress={handleCopy}
        disabled={!msg.tool_output}
      >
        <Text style={styles.toolText}>
          ⚙ {label}{dur}{ts ? ` · ${ts}` : ""}{msg.tool_output ? (open ? " ▲" : " ▼") : ""}{copied ? " · copied" : ""}
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
  const baseTextStyle = [styles.bubbleText, isUser && { color: "#052e16" }];
  return (
    <Pressable
      style={[styles.bubble, isUser ? styles.bubbleUser : styles.bubbleAgent]}
      onLongPress={handleCopy}
    >
      {isUser
        ? (
          <View style={{ gap: 4 }}>
            {msg.attachments && msg.attachments.length > 0 && (
              <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 4 }}>
                {msg.attachments.map((a, i) => (
                  <Text key={i} style={{ color: '#052e16', fontSize: 11, backgroundColor: 'rgba(255,255,255,0.3)', borderRadius: 8, paddingHorizontal: 6, paddingVertical: 2 }}>📎 {a.split('/').pop()}</Text>
                ))}
              </View>
            )}
            <Text style={baseTextStyle}>{msg.text}</Text>
          </View>
        )
        : renderMarkdown(msg.text, styles.bubbleText)}
      <View style={{ flexDirection: "row", justifyContent: "space-between", alignItems: "center", marginTop: 2 }}>
        <View style={{ flex: 1 }} />
        {copied ? (
          <Text style={{ fontSize: 10, color: isUser ? "#052e16" : "#4ade80" }}>copied ✓</Text>
        ) : (
          <Text style={{ fontSize: 10, color: isUser ? "#052e16" : "#4b5563", opacity: 0.5 }}>hold to copy</Text>
        )}
        {ts ? (
          <Text style={[styles.bubbleTs, { color: isUser ? "#052e16" : "#9ca3af" }]}>{ts}</Text>
        ) : null}
      </View>
    </Pressable>
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
  bubbleTs: { fontSize: 10, marginTop: 3, opacity: 0.7 },
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
  modelBar: {
    flexDirection: "row", alignItems: "center", gap: 10,
    marginHorizontal: 8, marginBottom: 6, paddingTop: 0,
  },
  modelChip: {
    backgroundColor: "#16161c", borderRadius: 8, borderWidth: 1,
    borderColor: "#2c2c36", paddingHorizontal: 10, paddingVertical: 5,
    flexShrink: 1, // let the model name yield space to the ctx chip
  },
  modelChipText: { color: "#4ade80", fontSize: 12, fontFamily: "JetBrainsMono NF Mono" },
  modelHint: { color: "#4b5563", fontSize: 11 },
  // row already has gap:10 — shrink instead of pushing off-screen
  ctxChip: { flexShrink: 1, borderColor: "#3f3f46" },
  ctxChipText: { color: "#fbbf24", fontSize: 11, fontFamily: "JetBrainsMono NF Mono" },
  pickerOverlay: {
    ...StyleSheet.absoluteFill, backgroundColor: "rgba(0,0,0,0.6)",
    justifyContent: "center", alignItems: "center",
  },
  picker: {
    width: "88%", maxHeight: "70%", backgroundColor: "#14151c",
    borderRadius: 12, borderWidth: 1, borderColor: "#26262e", overflow: "hidden",
  },
  pickerHeader: {
    flexDirection: "row", justifyContent: "space-between", alignItems: "center",
    paddingHorizontal: 12, paddingTop: 10, paddingBottom: 8,
    borderBottomWidth: 1, borderBottomColor: "#23232c",
  },
  pickerTitle: { color: "#9ca3af", fontSize: 12, fontWeight: "700" },
  pickerSearch: {
    color: "#f3f4f6", fontSize: 13, fontFamily: "JetBrainsMono NF Mono",
    backgroundColor: "#0f1016", borderRadius: 8, borderWidth: 1,
    borderColor: "#26262e", paddingHorizontal: 10, paddingVertical: 7,
    marginHorizontal: 10, marginTop: 8,
  },
  pickerList: { maxHeight: 360 },
  pickerRow: {
    paddingHorizontal: 12, paddingVertical: 10, borderBottomWidth: 1,
    borderBottomColor: "#1c1d24",
  },
  pickerRowActive: { backgroundColor: "#1a2018" },
  pickerRowText: { color: "#d1d5db", fontSize: 13, fontFamily: "JetBrainsMono NF Mono" },
  chatInputRow: {
    flexDirection: "column", borderTopWidth: 1, borderTopColor: "#23232c",
    padding: 8, paddingTop: 10, paddingBottom: 30, gap: 6,
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
