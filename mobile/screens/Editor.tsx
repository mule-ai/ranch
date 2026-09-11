// M10 — mobile editor: browse files on the daemon's machine, edit, save.
// .md/.mdx files get a Review tab rendering the markdown via `marked`.
import { useEffect, useRef, useState } from "react";
import {
  Alert,
  Keyboard,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { lexer, type Token, type Tokens } from "marked";
import { Frame, nextId } from "../lib/frames";
import { Relay } from "../lib/relay";

type Props = { relay: Relay; onExit: () => void };

type BrowseState = {
  path: string;
  parent: string | null;
  dirs: string[];
  files: string[];
};

type OpenFile = {
  path: string;
  original: string;
  mtime: number;
  size: number;
};

function isMarkdown(path: string): boolean {
  return /\.(md|mdx)$/i.test(path);
}

export function EditorScreen({ relay, onExit }: Props) {
  const [browse, setBrowse] = useState<BrowseState | null>(null);
  const [browseLoading, setBrowseLoading] = useState(false);
  const [openFile, setOpenFile] = useState<OpenFile | null>(null);
  const [draft, setDraft] = useState("");
  const [view, setView] = useState<"edit" | "review">("edit");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  // path of a file that changed on disk while the draft was dirty
  const [externChanged, setExternChanged] = useState<string | null>(null);

  // req_id matching so replies land in the right handler
  const dirReqRef = useRef<string | null>(null);
  const readReqRef = useRef<string | null>(null);
  const writeReqRef = useRef<string | null>(null);
  // live mirrors so the frame handler (subscribed once per relay) never
  // sees first-render closures
  const draftRef = useRef("");
  const openFileRef = useRef<OpenFile | null>(null);
  useEffect(() => {
    draftRef.current = draft;
  }, [draft]);
  useEffect(() => {
    openFileRef.current = openFile;
  }, [openFile]);

  const loadDir = (path?: string) => {
    if (!path && !browse) path = undefined;
    const rid = nextId();
    dirReqRef.current = rid;
    setBrowseLoading(true);
    setError(null);
    relay.send({ t: "DirList", id: nextId(), client: "mobile", req_id: rid, path } as Frame);
  };

  const loadFile = (name: string) => {
    const path = browse ? (browse.path === "/" ? "/" + name : browse.path + "/" + name) : name;
    const rid = nextId();
    readReqRef.current = rid;
    setBrowseLoading(true);
    setError(null);
    relay.send({ t: "FileRead", id: nextId(), client: "mobile", req_id: rid, path } as Frame);
  };

  const save = () => {
    if (!openFile || saving) return;
    const rid = nextId();
    writeReqRef.current = rid;
    setSaving(true);
    setError(null);
    relay.send({
      t: "FileWrite",
      id: nextId(),
      client: "mobile",
      req_id: rid,
      path: openFile.path,
      content: draft,
      mtime: openFile.mtime,
    } as Frame);
  };

  // Back: confirm discard when there are unsaved edits, else go to browser / exit
  const backTapped = () => {
    if (openFile && draft !== openFile.original) {
      Alert.alert("Unsaved changes", `Discard changes to ${openFile.path}?`, [
        { text: "Keep editing", style: "cancel" },
        { text: "Discard & close", style: "destructive", onPress: onExit },
      ]);
      return;
    }
    if (openFile) {
      setOpenFile(null);
      setView("edit");
      setError(null);
      setExternChanged(null);
    } else {
      onExit();
    }
  };

  const openFileFromPath = (path: string) => {
    const rid = nextId();
    readReqRef.current = rid;
    setError(null);
    relay.send({ t: "FileRead", id: nextId(), client: "mobile", req_id: rid, path } as Frame);
  };

  const kbHeight = useKbHeight();

  useEffect(() => {
    const unlisten = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "DirListOk":
          if (f.req_id === dirReqRef.current) {
            dirReqRef.current = null;
            setBrowse({ path: f.path, parent: f.parent ?? null, dirs: f.dirs, files: f.files ?? [] });
            setBrowseLoading(false);
          }
          break;
        case "FileReadOk":
          if (f.req_id === readReqRef.current) {
            readReqRef.current = null;
            setBrowseLoading(false);
            setOpenFile({ path: f.path, original: f.content, mtime: f.mtime, size: f.size });
            setDraft(f.content);
            setView(isMarkdown(f.path) ? "review" : "edit");
          }
          break;
        case "FileWriteOk":
          if (f.req_id === writeReqRef.current) {
            writeReqRef.current = null;
            setSaving(false);
            setOpenFile((of) =>
              of ? { ...of, original: draftRef.current, mtime: f.mtime } : of
            );
            setSaved(true);
            setTimeout(() => setSaved(false), 2000);
          }
          break;
        case "FileChanged": {
          const of = openFileRef.current;
          if (of && of.path === f.path) {
            if (draftRef.current === of.original) {
              // no local edits: refresh silently (also updates mtime)
              openFileFromPath(f.path);
            } else {
              // local edits: surface a conflict banner, don't touch the draft
              setExternChanged(f.path);
            }
          }
          break;
        }
        case "Error":
          if (f.req_id === dirReqRef.current) {
            dirReqRef.current = null;
            setBrowseLoading(false);
            setError(f.message);
          } else if (f.req_id === readReqRef.current) {
            readReqRef.current = null;
            setBrowseLoading(false);
            setError(f.message);
          } else if (f.req_id === writeReqRef.current) {
            writeReqRef.current = null;
            setSaving(false);
            setError(f.message);
            // conflict: prompt reload
            if (f.message.startsWith("file changed on disk") && openFileRef.current) {
              const p = openFileRef.current.path;
              Alert.alert(
                "File changed on disk",
                "Someone else saved this file since you opened it.",
                [
                  { text: "Cancel", style: "cancel" },
                  { text: "Reload", onPress: () => openFileFromPath(p) },
                ]
              );
            }
          }
          break;
      }
    });
    return () => {
      unlisten();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [relay]);

  // initial browse: $HOME
  const booted = useRef(false);
  useEffect(() => {
    if (!booted.current) {
      booted.current = true;
      loadDir();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [relay]);

  const dirty = openFile ? draft !== openFile.original : false;
  const showMarkdownTabs = openFile && isMarkdown(openFile.path);

  return (
    <View style={[styles.wrap, { paddingBottom: kbHeight }]}>
      <View style={styles.header}>
        <Pressable onPress={backTapped} hitSlop={8}>
          <Text style={styles.back}>{openFile ? "‹ files" : "‹ back"}</Text>
        </Pressable>
        <Text style={styles.title} numberOfLines={1}>
          {openFile ? openFile.path : "files"}
        </Text>
        {openFile && (
          <View style={styles.headerRight}>
            {dirty ? <Text style={styles.dirty}>●</Text> : saved ? <Text style={styles.saved}>✓</Text> : null}
            <Pressable
              style={[styles.saveBtn, !dirty && styles.saveBtnIdle, saving && styles.saveBtnSaving]}
              onPress={save}
              disabled={!dirty || saving}
            >
              <Text style={styles.saveText}>{saving ? "…" : "save"}</Text>
            </Pressable>
          </View>
        )}
      </View>

      {error !== null && (
        <View style={styles.errBanner}>
          <Text style={styles.errText}>{error}</Text>
          <Pressable onPress={() => setError(null)} hitSlop={8}>
            <Text style={styles.errClose}>✕</Text>
          </Pressable>
        </View>
      )}

      {externChanged !== null && openFile && externChanged === openFile.path && (
        <View style={styles.warnBanner}>
          <Text style={styles.warnText}>changed on disk — reload?</Text>
          <View style={styles.warnBtns}>
            <Pressable onPress={() => openFileFromPath(externChanged)} hitSlop={8}>
              <Text style={styles.warnReload}>reload</Text>
            </Pressable>
            <Pressable onPress={() => setExternChanged(null)} hitSlop={8}>
              <Text style={styles.warnDismiss}>keep mine</Text>
            </Pressable>
          </View>
        </View>
      )}

      {showMarkdownTabs && (
        <View style={styles.tabs}>
          <Pressable style={[styles.tab, view === "edit" && styles.tabOn]} onPress={() => setView("edit")}>
            <Text style={[styles.tabText, view === "edit" && styles.tabTextOn]}>edit</Text>
          </Pressable>
          <Pressable style={[styles.tab, view === "review" && styles.tabOn]} onPress={() => setView("review")}>
            <Text style={[styles.tabText, view === "review" && styles.tabTextOn]}>review</Text>
          </Pressable>
        </View>
      )}

      {browseLoading && !openFile ? (
        <Text style={styles.dim}>loading…</Text>
      ) : openFile ? (
        view === "edit" ? (
          <TextInput
            style={styles.editor}
            value={draft}
            onChangeText={setDraft}
            multiline
            scrollEnabled
            textAlignVertical="top"
            autoCapitalize="none"
            autoCorrect={false}
            autoComplete="off"
            spellCheck={false}
            selectionColor="#4ade80"
          />
        ) : (
          <ScrollView style={styles.review} contentContainerStyle={styles.reviewContent}>
            <MarkdownView source={draft} />
          </ScrollView>
        )
      ) : browse ? (
        <View style={styles.browse}>
          {browse.parent !== null && (
            <Pressable style={styles.row} onPress={() => loadDir(browse.parent ?? undefined)}>
              <Text style={styles.rowTitle}>../</Text>
              <Text style={styles.dim}>up</Text>
            </Pressable>
          )}
          {browse.dirs.map((d) => (
            <Pressable key={d} style={styles.row} onPress={() => loadDir(browse.path === "/" ? "/" + d : browse.path + "/" + d)}>
              <Text style={styles.rowTitle}>{d}/</Text>
              <Text style={styles.dim}>open</Text>
            </Pressable>
          ))}
          {browse.files.map((f) => (
            <Pressable key={f} style={styles.row} onPress={() => loadFile(f)}>
              <Text style={styles.rowTitle}>{f}</Text>
              {isMarkdown(f) && <Text style={styles.mdTag}>md</Text>}
            </Pressable>
          ))}
          {browse.dirs.length === 0 && browse.files.length === 0 && (
            <Text style={styles.dim}>empty directory</Text>
          )}
        </View>
      ) : (
        <Text style={styles.dim}>no directory selected</Text>
      )}
    </View>
  );
}

// keyboard height (edge-to-edge Android): pad the container manually
function useKbHeight(): number {
  const [h, setH] = useState(0);
  useEffect(() => {
    const show = Keyboard.addListener("keyboardDidShow", (e) => setH(e.endCoordinates?.height ?? 0));
    const hide = Keyboard.addListener("keyboardDidHide", () => setH(0));
    return () => {
      show.remove();
      hide.remove();
    };
  }, []);
  return h;
}

// ---------- markdown rendering (marked tokens → RN views) ----------

function MarkdownView({ source }: { source: string }) {
  const tokens = lexer(source, { gfm: true });
  return (
    <View>
      {tokens.map((t, i) => (
        <Block key={i} token={t} />
      ))}
    </View>
  );
}

function Block({ token }: { token: Token }) {
  switch (token.type) {
    case "heading": {
      const h = token as Tokens.Heading;
      const sizes = [26, 22, 19, 16, 14, 13];
      return (
        <Text style={{ color: "#f3f4f6", fontSize: sizes[Math.min(h.depth - 1, 5)], fontWeight: "700", marginTop: 10, marginBottom: 6 }}>
          <Inline tokens={h.tokens} />
        </Text>
      );
    }
    case "paragraph": {
      const p = token as Tokens.Paragraph;
      return (
        <Text style={{ color: "#d1d5db", fontSize: 15, lineHeight: 22, marginBottom: 10 }}>
          <Inline tokens={p.tokens} />
        </Text>
      );
    }
    case "blockquote": {
      const q = token as Tokens.Blockquote;
      return (
        <View style={{ borderLeftWidth: 3, borderLeftColor: "#4ade80", paddingLeft: 10, marginVertical: 8, opacity: 0.85 }}>
          {q.tokens.map((t, i) => (
            <Block key={i} token={t} />
          ))}
        </View>
      );
    }
    case "code": {
      const c = token as Tokens.Code;
      return (
        <View style={styles.codeBlock}>
          {c.lang && <Text style={styles.codeLang}>{c.lang}</Text>}
          <Text style={styles.codeText}>{c.text}</Text>
        </View>
      );
    }
    case "list": {
      const l = token as Tokens.List;
      return (
        <View style={{ marginVertical: 8 }}>
          {l.items.map((item, i) => (
            <View key={i} style={{ flexDirection: "row", marginBottom: 3, paddingLeft: 12 }}>
              <Text style={{ color: "#4ade80", fontSize: 15 }}>
                {l.ordered ? `${Number(l.start ?? 1) + i}. ` : "• "}
              </Text>
              <Text style={{ color: "#d1d5db", fontSize: 15, lineHeight: 22, flex: 1 }}>
                <Inline tokens={item.tokens} />
              </Text>
            </View>
          ))}
        </View>
      );
    }
    case "hr":
      return <View style={{ height: 1, backgroundColor: "#2a2a34", marginVertical: 10 }} />;
    case "table": {
      const t = token as Tokens.Table;
      return (
        <View style={{ marginVertical: 8, borderWidth: 1, borderColor: "#2a2a34", borderRadius: 6, overflow: "hidden" }}>
          <View style={{ flexDirection: "row", backgroundColor: "#1a1b23", borderBottomWidth: 1, borderBottomColor: "#2a2a34" }}>
            {t.header.map((h, i) => (
              <Text key={i} style={{ color: "#f3f4f6", fontSize: 13, fontWeight: "700", paddingHorizontal: 8, paddingVertical: 6, flex: 1 }}>
                <Inline tokens={h.tokens} />
              </Text>
            ))}
          </View>
          {t.rows.map((row, ri) => (
            <View key={ri} style={{ flexDirection: "row", borderBottomWidth: ri < t.rows.length - 1 ? 1 : 0, borderBottomColor: "#1f2430" }}>
              {row.map((cell, ci) => (
                <Text key={ci} style={{ color: "#d1d5db", fontSize: 13, paddingHorizontal: 8, paddingVertical: 6, flex: 1 }}>
                  <Inline tokens={cell.tokens} />
                </Text>
              ))}
            </View>
          ))}
        </View>
      );
    }
    case "text": {
      const tx = token as Tokens.Text;
      return (
        <Text style={{ color: "#d1d5db", fontSize: 15, lineHeight: 22, marginBottom: 10 }}>
          <Inline tokens={tx.tokens ?? []} />
        </Text>
      );
    }
    case "space":
      return null;
    default:
      return null;
  }
}

function Inline({ tokens }: { tokens: Token[] }) {
  const out: React.ReactNode[] = [];
  tokens.forEach((t, i) => {
    switch (t.type) {
      case "text": {
        const tx = t as Tokens.Text;
        out.push(<Text key={i}>{tx.raw}</Text>);
        break;
      }
      case "strong":
        out.push(
          <Text key={i} style={{ fontWeight: "700" }}>
            <Inline tokens={(t as Tokens.Strong).tokens} />
          </Text>
        );
        break;
      case "em":
        out.push(
          <Text key={i} style={{ fontStyle: "italic" }}>
            <Inline tokens={(t as Tokens.Em).tokens} />
          </Text>
        );
        break;
      case "del":
        out.push(
          <Text key={i} style={{ textDecorationLine: "line-through", opacity: 0.7 }}>
            <Inline tokens={(t as Tokens.Del).tokens} />
          </Text>
        );
        break;
      case "codespan":
        out.push(
          <Text key={i} style={{ fontFamily: "JetBrainsMono NF Mono", backgroundColor: "#1f2430", paddingHorizontal: 3, fontSize: 13, color: "#4ade80" }}>
            {(t as Tokens.Codespan).text}
          </Text>
        );
        break;
      case "link": {
        const l = t as Tokens.Link;
        out.push(
          <Text key={i} style={{ color: "#60a5fa", textDecorationLine: "underline" }}>
            <Inline tokens={l.tokens} />
          </Text>
        );
        break;
      }
      case "br":
        out.push(<Text key={i}>{"\n"}</Text>);
        break;
      case "image": {
        const img = t as Tokens.Image;
        out.push(<Text key={i}>🖼 {img.text || img.href}</Text>);
        break;
      }
      default:
        out.push(<Text key={i}>{(t as Tokens.Text).raw ?? ""}</Text>);
    }
  });
  return <>{out}</>;
}

const styles = StyleSheet.create({
  wrap: { flex: 1, backgroundColor: "#101014", paddingTop: 56, paddingHorizontal: 12 },
  header: {
    flexDirection: "row", alignItems: "center", gap: 10,
    paddingBottom: 8, borderBottomWidth: 1, borderBottomColor: "#1f2430",
  },
  back: { color: "#4ade80", fontSize: 15, width: 70 },
  title: { color: "#f3f4f6", fontWeight: "700", fontSize: 15, flex: 1 },
  headerRight: { flexDirection: "row", alignItems: "center", gap: 8 },
  dirty: { color: "#f59e0b", fontSize: 16 },
  saved: { color: "#4ade80", fontSize: 14 },
  saveBtn: {
    backgroundColor: "#16a34a", borderRadius: 8, paddingHorizontal: 14,
    paddingVertical: 5, justifyContent: "center",
  },
  saveBtnIdle: { backgroundColor: "#1f2430" },
  saveBtnSaving: { opacity: 0.6 },
  saveText: { color: "#fff", fontWeight: "700", fontSize: 13 },
  errBanner: {
    flexDirection: "row", alignItems: "center", justifyContent: "space-between",
    backgroundColor: "rgba(248,113,113,0.1)", borderRadius: 8,
    paddingHorizontal: 10, paddingVertical: 8, marginTop: 8, gap: 8,
  },
  errText: { color: "#f87171", fontSize: 13, flex: 1 },
  errClose: { color: "#f87171", fontSize: 14 },
  warnBanner: {
    flexDirection: "row", alignItems: "center", justifyContent: "space-between",
    backgroundColor: "rgba(245,158,11,0.12)", borderRadius: 8,
    paddingHorizontal: 10, paddingVertical: 8, marginTop: 8, gap: 8,
  },
  warnText: { color: "#f59e0b", fontSize: 13, flex: 1 },
  warnBtns: { flexDirection: "row", gap: 12 },
  warnReload: { color: "#f59e0b", fontSize: 13, fontWeight: "700" },
  warnDismiss: { color: "#9ca3af", fontSize: 13 },
  tabs: { flexDirection: "row", gap: 6, marginTop: 8 },
  tab: {
    borderWidth: 1, borderColor: "#374151", borderRadius: 999,
    paddingHorizontal: 14, paddingVertical: 4,
  },
  tabOn: { borderColor: "#4ade80", backgroundColor: "rgba(74,222,128,0.12)" },
  tabText: { color: "#9ca3af", fontSize: 13 },
  tabTextOn: { color: "#4ade80", fontWeight: "600" },
  browse: { flex: 1, marginTop: 4 },
  row: {
    flexDirection: "row", alignItems: "center", justifyContent: "space-between",
    paddingVertical: 13, borderBottomWidth: 1, borderBottomColor: "#1f2430",
  },
  rowTitle: { color: "#f3f4f6", fontSize: 16, fontWeight: "600", flexShrink: 1 },
  mdTag: { color: "#4ade80", fontSize: 12, fontWeight: "700", marginLeft: 8 },
  dim: { color: "#6b7280", padding: 12, fontSize: 14 },
  editor: {
    flex: 1, color: "#d1d5db", fontFamily: "JetBrainsMono NF Mono",
    fontSize: 14, lineHeight: 20, paddingTop: 12, paddingHorizontal: 4,
    backgroundColor: "#0a0a0e",
  },
  review: { flex: 1, backgroundColor: "#0a0a0e" },
  reviewContent: { padding: 12, paddingBottom: 40 },
  codeBlock: {
    backgroundColor: "#14141a", borderRadius: 8, paddingHorizontal: 10,
    paddingVertical: 8, marginVertical: 6, borderWidth: 1, borderColor: "#23232c",
  },
  codeLang: { color: "#6b7280", fontSize: 11, marginBottom: 4 },
  codeText: { color: "#e5e7eb", fontFamily: "JetBrainsMono NF Mono", fontSize: 13, lineHeight: 18 },
});
