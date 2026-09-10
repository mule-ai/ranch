//! Ranch wire protocol (v0).
//!
//! One frame schema for every transport. Frames are JSON objects, one per
//! line (JSON-lines). Locally over a unix socket; over the relay each frame
//! is the `payload` of a Realtime broadcast.
//!
//! MVP payload is **rows as plain strings** (formatter PLAIN diff), not
//! styled cells — see SPEC §11.1 note 1. This keeps clients thin and lets
//! M2 swap in styled cells without changing framing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const PROTO_VERSION: u32 = 0;

/// A frame is a JSON object. Serde externally-tags on the `t` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum Frame {
    /// Client -> daemon: identify.
    Hello {
        id: String,
        client: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        caps: Vec<String>,
    },
    /// Daemon -> client: ack + current session list.
    HelloOk {
        id: String,
        machine: String,
        sessions: Vec<SessionMeta>,
    },
    /// Client -> daemon: attach to a session (and optionally a pane).
    Attach {
        id: String,
        client: String,
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
    },
    /// Client -> daemon: stop receiving updates.
    Detach {
        id: String,
        client: String,
    },
    /// Daemon -> client: full state of a session's panes. May be chunked.
    Snapshot {
        id: String,
        client: String,
        session: String,
        seq: u64,
        /// Active window's split tree (kept for pre-window clients).
        layout: Layout,
        active_pane: String,
        panes: Vec<PaneSnap>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        meta: Vec<PaneMeta>,
        /// Window stack (M5). Absent from single-window snapshots of
        /// older daemons; clients treat a missing field as one window.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        windows: Vec<WindowSnap>,
        /// Active window id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<String>,
    },
    /// Daemon -> client: incremental row updates for one pane. Droppable:
    /// a newer update for the same pane supersedes an older one.
    Update {
        id: String,
        client: String,
        session: String,
        pane: String,
        seq: u64,
        cols: u16,
        rows: u16,
        /// `(row_index, text)` pairs for rows that changed.
        rows_upd: Vec<(u16, String)>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cursor: Option<Cursor>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Client -> daemon: terminal input bytes (already ANSI-encoded) to a pane.
    Input {
        id: String,
        client: String,
        session: String,
        pane: String,
        /// base64-encoded bytes
        data: String,
    },
    /// Client -> daemon: change the canonical pane size for a session.
    Resize {
        id: String,
        client: String,
        session: String,
        cols: u16,
        rows: u16,
    },
    /// Client -> daemon: request paged scrollback history.
    ScrollbackReq {
        id: String,
        client: String,
        session: String,
        pane: String,
        /// offset within the history ring (0 = oldest available)
        offset: u64,
        limit: u32,
    },
    /// Daemon -> client: paged scrollback lines.
    Scrollback {
        id: String,
        client: String,
        session: String,
        pane: String,
        offset: u64,
        lines: Vec<String>,
    },
    /// Control: create a new shell session.
    SessionsCreate {
        req_id: String,
        name: Option<String>,
        /// "shell" (default) or "forge" — a forge session's panes run the
        /// `pi` agent instead of a bare shell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Working directory for the session's first pane (default $HOME).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// kind="forge" only: adopt an EXISTING forge session instead of
        /// creating one (resume). The pane binds to it and the SSE watch
        /// replays history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forge_session: Option<String>,
    },
    /// Client -> daemon: list directories at `path` (None = $HOME).
    /// Local-machine filesystem access for mobile dir pickers.
    DirList {
        id: String,
        client: String,
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Daemon -> client: directory listing.
    DirListOk {
        id: String,
        req_id: String,
        /// the resolved absolute path
        path: String,
        /// parent dir, when there is one (up-navigation)
        parent: Option<String>,
        /// subdirectory names, sorted
        dirs: Vec<String>,
    },
    /// Client -> daemon: list resumable forge sessions.
    ForgeList {
        id: String,
        client: String,
        /// echoed back in ForgeListOk so clients match the reply
        req_id: String,
    },
    /// Daemon -> client: forge sessions available for resume.
    ForgeListOk {
        id: String,
        req_id: String,
        sessions: Vec<ForgeSessionInfo>,
    },
    /// Daemon -> client: ack a create/split with the new ids.
    SessionsAck {
        req_id: String,
        session: String,
        pane: String,
    },
    SessionsRename {
        session: String,
        name: String,
    },
    SessionsKill {
        session: String,
    },
    SessionsSelect {
        session: String,
        pane: String,
    },
    PaneSplit {
        req_id: String,
        session: String,
        pane: String,
        /// 0 = horizontal (side-by-side), 1 = vertical (stacked)
        dir: u8,
        /// absent/"shell" = PTY pane; "forge" = chat pane bound to a
        /// NEW forge session (agent split, M8)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    /// Client -> daemon: resize the split containing `pane`. The daemon
    /// adjusts the split ratio of the smallest enclosing split that has
    /// room. delta is in cells; positive grows `pane`'s side.
    PaneResize {
        session: String,
        pane: String,
        /// 0 = horizontal (top/bottom), 1 = vertical (left/right) axis
        dir: u8,
        delta: i16,
    },
    PaneKill {
        session: String,
        pane: String,
    },
    /// Client -> daemon: swap the positions of two panes in the layout
    /// tree. Each pane keeps its own PTY/VT state; only the rectangles
    /// trade places. The daemon resizes both PTYs to their new geometry.
    PaneSwap {
        session: String,
        a: String,
        b: String,
    },
    // ----- windows (M5) -----
    /// Client -> daemon: create a window (one new pane, auto-named).
    WindowNew {
        req_id: String,
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Client -> daemon: make `window` (id) active.
    WindowSelect {
        session: String,
        window: String,
    },
    /// Client -> daemon: move `delta` windows around the ring.
    WindowNext {
        session: String,
        delta: i16,
    },
    /// Client -> daemon: kill a window (and its panes). Killing the
    /// last window kills the session.
    WindowKill {
        session: String,
        window: String,
    },
    /// Client -> daemon: rename a window.
    WindowRename {
        session: String,
        window: String,
        name: String,
    },
    // ----- forge chat panes (M8) -----
    /// Client -> daemon: send a chat message to a forge-chat pane.
    /// The daemon POSTs to the forge API on its worker thread; the
    /// resulting rows come back as `chat` broadcasts.
    ChatSend {
        id: String,
        client: String,
        session: String,
        pane: String,
        text: String,
    },
    /// Daemon -> client: conversation rows for a forge-chat pane.
    /// `reset` replaces whatever the client has (snapshot semantics);
    /// otherwise `msgs` are appends in sequence order.
    Chat {
        id: String,
        session: String,
        pane: String,
        msgs: Vec<ChatMsg>,
        #[serde(default)]
        reset: bool,
    },
    /// Out-of-band status (no screen change).
    Meta {
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    /// Error response to a failed request.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        req_id: Option<String>,
        message: String,
    },
    /// Heartbeat.
    Hb,
    /// A chunk of a split frame (reassembly on the receiving side).
    Chunk {
        chunk_id: String,
        i: u32,
        n: u32,
        data: String,
    },
}

/// One row of a forge agent conversation (M8). Mirrors a forge
/// `messages` row: user prompts, assistant replies, and tool calls
/// (linked by `tool_call_id`) in `sequence` order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMsg {
    pub seq: i64,
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// One window in a session's window stack (M5). A window is a named
/// layout tree; exactly one is active per session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowSnap {
    pub id: String,
    pub name: String,
    pub layout: Layout,
}

/// A resumable forge session (ForgeListOk row).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForgeSessionInfo {
    pub id: String,
    pub title: String,
    /// last_active timestamp string as forge reports it
    pub updated: String,
    /// forge's ended_at, when the session was severed/ended
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub kind: String, // "shell" | "forge" | "mule"
    pub active_pane: String,
    pub panes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
    /// Window names in order (M5; absent from older daemons).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneMeta {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneSnap {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
    /// Full screen, row-major, plain text (trimmed).
    pub lines: Vec<String>,
    /// The pane's update sequence at snapshot time. Clients seed their
    /// per-pane dedup/gap tracking from this so reordered or duplicated
    /// updates (Realtime broadcast makes no ordering guarantee) are
    /// dropped instead of overwriting fresh rows with stale content.
    #[serde(default)]
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// "pty" (default) or "forge-chat" (M8). Chat panes carry `chat`
    /// instead of terminal rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Full conversation for forge-chat panes (absent for PTY panes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<Vec<ChatMsg>>,
    /// For forge-chat panes: the forge session uuid this pane is bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge_session: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

/// A binary split tree. Each leaf is a pane; the daemon computes each pane's
/// (cols, rows) from the session's canonical size by traversing this tree
/// with 50/50 splits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "k")]
pub enum Layout {
    Leaf { pane: String },
    Split {
        dir: u8,
        a: Box<Layout>,
        b: Box<Layout>,
        /// Percent of the space given to `a` (1..=99). Defaults to 50;
        /// ignored on the wire when absent (older clients).
        #[serde(default = "default_pct")]
        pct: u8,
    },
}

fn default_pct() -> u8 {
    50
}

/// Pane ids in layout traversal order (depth-first, left/top first).
/// Clients use this for tmux-style prev/next-pane operations.
pub fn leaf_order(l: &Layout) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(l: &Layout, out: &mut Vec<String>) {
        match l {
            Layout::Leaf { pane } => out.push(pane.clone()),
            Layout::Split { a, b, .. } => {
                walk(a, out);
                walk(b, out);
            }
        }
    }
    walk(l, &mut out);
    out
}

// ---------- base64 (no external deps for the spike-grade path) ----------

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

fn b64_val(c: u8) -> Option<u32> {
    B64.iter().position(|&b| b == c).map(|p| p as u32)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s: Vec<u8> = s.bytes().filter(|&b| b != b'\n' && b != b'\r').collect();
    if s.is_empty() {
        return Some(Vec::new());
    }
    if s.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for i in (0..s.len()).step_by(4) {
        let a = b64_val(s[i])?;
        let b = b64_val(s[i + 1])?;
        let c = if s[i + 2] == b'=' { 0 } else { b64_val(s[i + 2])? };
        let d = if s[i + 3] == b'=' { 0 } else { b64_val(s[i + 3])? };
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((n >> 16) as u8);
        if s[i + 2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if s[i + 3] != b'=' {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ---------- framing / chunking ----------

/// Max bytes for a single on-the-wire frame (JSON-line) before we chunk.
/// Chosen to stay well under the Realtime broadcast cap (~28 KB).
pub const MAX_FRAME: usize = 16 * 1024;

/// Serialize a frame into one or more JSON-lines. If the serialized frame is
/// under `MAX_FRAME`, returns a single line. Otherwise returns `n` chunk
/// frames whose `data` fields reassemble to the original JSON.
pub fn encode_frame(frame: &Frame, chunk_id: &str) -> Vec<String> {
    let json = serde_json::to_string(frame).expect("frame serializes");
    if json.len() <= MAX_FRAME {
        return vec![json];
    }
    let mut out = Vec::new();
    let n = (json.len() + MAX_FRAME - 1) / MAX_FRAME;
    for (i, piece) in json.as_bytes().chunks(MAX_FRAME).enumerate() {
        let f = Frame::Chunk {
            chunk_id: chunk_id.to_string(),
            i: i as u32,
            n: n as u32,
            data: String::from_utf8_lossy(piece).into_owned(),
        };
        out.push(serde_json::to_string(&f).expect("chunk serializes"));
    }
    out
}

/// Incremental frame decoder. Feed it raw bytes (which may contain zero, one,
/// or many partial/complete JSON-lines, interleaved with chunk frames). It
/// reassembles chunked frames and yields complete `Frame`s.
pub struct Decoder {
    buf: Vec<u8>,
    pending: HashMap<String, (u32, Vec<Option<String>>)>,
}

impl Decoder {
    pub fn new() -> Self {
        Self { buf: Vec::new(), pending: HashMap::new() }
    }

    /// Feed a chunk of bytes; returns any complete frames decoded from it.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        self.buf.extend_from_slice(bytes);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // drop the \n
            let line = std::str::from_utf8(&line).unwrap_or("");
            if line.is_empty() {
                continue;
            }
            if let Ok(f) = serde_json::from_str::<Frame>(line) {
                if let Some(frame) = self.maybe_complete(&f) {
                    frames.push(frame);
                }
            }
        }
        frames
    }

    /// If `f` is the last chunk of a pending frame, reassemble and return it.
    fn maybe_complete(&mut self, f: &Frame) -> Option<Frame> {
        match f {
            Frame::Chunk { chunk_id, i, n, data } => {
                let complete = {
                    let entry = self.pending.entry(chunk_id.clone()).or_insert_with(|| {
                        (*n, vec![None; *n as usize])
                    });
                    let slots = &mut entry.1;
                    if (*i as usize) < slots.len() && slots[*i as usize].is_none() {
                        slots[*i as usize] = Some(data.clone());
                    }
                    slots.iter().all(|s| s.is_some())
                };
                if complete {
                    let joined: String = self.pending
                        .get(chunk_id)
                        .map(|(_, slots)| slots.iter().map(|s| s.as_deref().unwrap_or("")).collect())
                        .unwrap_or_default();
                    self.pending.remove(chunk_id);
                    serde_json::from_str::<Frame>(&joined).ok()
                } else {
                    None
                }
            }
            other => Some(other.clone()),
        }
    }

    /// Number of bytes still buffered (for diagnostics).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Attempt to parse any remaining buffered line (no trailing newline yet).
    /// Call at stream end (EOF / disconnect).
    pub fn flush(&mut self) -> Vec<Frame> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let buf: Vec<u8> = std::mem::take(&mut self.buf);
        let line = std::str::from_utf8(&buf).unwrap_or("");
        match serde_json::from_str::<Frame>(line) {
            Ok(f) => self.maybe_complete(&f),
            Err(_) => None,
        }
        .into_iter()
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_frames_one_feed() {
        let hello = Frame::Hello {
            id: "h1".into(),
            client: "py".into(),
            caps: vec![],
        };
        let attach = Frame::Attach {
            id: "a1".into(),
            client: "py".into(),
            session: "s1".into(),
            pane: None,
        };
        let mut buf = Vec::new();
        for line in encode_frame(&hello, "py") {
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
        for line in encode_frame(&attach, "py") {
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
        let mut d = Decoder::new();
        let frames = d.feed(&buf);
        assert_eq!(frames, vec![hello, attach]);
    }

    #[test]
    fn roundtrip_small() {
        let f = Frame::Hb;
        for line in encode_frame(&f, "c1") {
            let mut d = Decoder::new();
            let mut payload = line;
            payload.push('\n');
            let frames = d.feed(payload.as_bytes());
            assert_eq!(frames, vec![f.clone()]);
        }
    }

    #[test]
    fn roundtrip_chunked() {
        let big = "x".repeat(40_000);
        let f = Frame::Update {
            id: "i1".into(),
            client: "c".into(),
            session: "s".into(),
            pane: "p".into(),
            seq: 1,
            cols: 80,
            rows: 24,
            rows_upd: vec![(0, big)],
            cursor: None,
            title: None,
        };
        let lines = encode_frame(&f, "c1");
        assert!(lines.len() > 1);
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for l in &lines {
            let mut payload = l.clone();
            payload.push('\n');
            got.extend(d.feed(payload.as_bytes()));
        }
        assert_eq!(got, vec![f]);
    }

    #[test]
    fn roundtrip_chunked_split_across_feeds() {
        let big = "y".repeat(40_000);
        let f = Frame::Snapshot {
            id: "i1".into(),
            client: "c".into(),
            session: "s".into(),
            seq: 1,
            layout: Layout::Leaf { pane: "p".into() },
            active_pane: "p".into(),
            panes: vec![PaneSnap {
                id: "p".into(),
                cols: 80,
                rows: 24,
                lines: vec![big; 24],
                seq: 7,
                cursor: None,
            }],
            meta: vec![],
        };
        // Serialize everything into one byte blob, feed byte-by-byte.
        let blob: String = encode_frame(&f, "c9").join("\n");
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for b in blob.bytes() {
            got.extend(d.feed(&[b]));
        }
        got.extend(d.flush());
        assert_eq!(got, vec![f]);
    }

    #[test]
    fn b64_roundtrip() {
        let data: Vec<u8> = (0..255u8).collect();
        let enc = b64_encode(&data);
        let dec = b64_decode(&enc).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn decoder_handles_interleaved_lines() {
        let f1 = Frame::Hb;
        let f2 = Frame::SessionsKill { session: "s".into() };
        let mut payload = String::new();
        payload.push_str(&serde_json::to_string(&f1).unwrap());
        payload.push('\n');
        payload.push_str(&serde_json::to_string(&f2).unwrap());
        payload.push('\n');
        let mut d = Decoder::new();
        let frames = d.feed(payload.as_bytes());
        assert_eq!(frames, vec![f1, f2]);
    }
}
