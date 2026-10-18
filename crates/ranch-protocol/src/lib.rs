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
use serde_json::Value;
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
        /// daemon build version (git sha based, e.g. "main-82c0643";
        /// "dev" for local builds). Clients compare with their own
        /// version + the dist release index to surface update banners.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// Whether the daemon is monitoring external pi sessions.
        #[serde(default)]
        monitor_external_pi: bool,
    },
    /// Client -> daemon: attach to a session (and optionally a pane).
    Attach {
        id: String,
        client: String,
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        /// When set, snapshots sent to this client truncate each chat
        /// pane's `chat` array to the last N rows; older rows are
        /// paged on demand via `ChatHistory`. Remote/mobile clients use
        /// this to avoid shipping megabyte snapshots over Realtime.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_limit: Option<usize>,
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
        /// kind="forge" only: use this agent profile (agent builder
        /// launch). Absent = daemon default (config or first profile).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        /// Working directory for the session's first pane (default $HOME).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// kind="forge" only: adopt an EXISTING forge session instead of
        /// creating one (resume). The pane binds to it and the SSE watch
        /// replays history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forge_session: Option<String>,
        /// kind="pi" only: path to an existing pi session file to resume.
        /// The daemon spawns a new pi child and calls switch_session into it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pi_session_file: Option<String>,
    },
    /// Client -> daemon: list directories at `path` (None = $HOME).
    /// Local-machine filesystem access for mobile dir pickers.
    DirList {
        id: String,
        client: String,
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// list hidden entries (dotfiles/dotdirs) too. Off by default —
        /// the daemon always hides hidden entries unless a client opts in.
        #[serde(default)]
        hidden: bool,
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
        /// regular file names, sorted (M10: editor file picker)
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },
    /// Client -> daemon: read a file's text content (M10 editor).
    FileRead {
        id: String,
        client: String,
        /// echoed back in FileReadOk so clients match the reply
        req_id: String,
        path: String,
    },
    /// Daemon -> client: a file's contents.
    FileReadOk {
        id: String,
        req_id: String,
        path: String,
        /// UTF-8 file contents
        content: String,
        /// last-modified unix timestamp (seconds); used for conflict detection
        mtime: i64,
        /// size in bytes
        size: u64,
    },
    /// Client -> daemon: write a file's text content (M10 editor).
    /// Atomic write (temp + rename). `mtime` (if present) must match the
    /// on-disk value or the write is refused (no silent clobber).
    FileWrite {
        id: String,
        client: String,
        /// echoed back in FileWriteOk so clients match the reply
        req_id: String,
        path: String,
        content: String,
        /// last-modified ts the client read with; None = skip conflict check
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<i64>,
    },
    /// Daemon -> client: a file was written successfully.
    FileWriteOk {
        id: String,
        req_id: String,
        path: String,
        /// new last-modified unix timestamp (seconds)
        mtime: i64,
    },
    /// Daemon -> client: a watched file changed on disk (M10 phase 3).
    /// The daemon auto-watches every file a client reads (FileRead) and
    /// refreshes the baseline on FileWriteOk; this is the push so the
    /// editor can surface a conflict immediately instead of only at save.
    /// `mtime` is the new last-modified unix timestamp (0 = file gone).
    FileChanged {
        path: String,
        mtime: i64,
    },
    /// Client -> daemon: upload a file from a remote client (mobile)
    /// to the daemon's uploads dir. `name` is the suggested file name
    /// (sanitized by the daemon); `b64` is base64-encoded content.
    /// Large payloads may arrive chunked (reassembled before decode).
    FilePut {
        id: String,
        client: String,
        /// echoed back in FilePutOk so clients match the reply
        req_id: String,
        name: String,
        b64: String,
    },
    /// Daemon -> client: the uploaded file was stored; `path` is the
    /// absolute path on the daemon machine (usable as a chat attachment).
    FilePutOk {
        id: String,
        req_id: String,
        path: String,
        size: u64,
    },
    /// Client -> daemon: fetch the bytes of an image on the daemon
    /// machine so the client can render it. `path` must be a *media*
    /// path: a file stored via `FilePut`, a `ChatSend` attachment, or
    /// an image the agent read during chat (see `ChatMsg.image_refs`).
    /// The daemon registers exactly those paths in its media registry
    /// — arbitrary file reads over the relay are refused. Images only
    /// (non-image media → `error` frame); capped at 16 MiB.
    FileGet {
        id: String,
        client: String,
        /// echoed back in `FileGetOk` / `error` so clients match the reply
        req_id: String,
        path: String,
    },
    /// Daemon -> client: the requested image bytes, base64-encoded.
    /// May be chunked on the wire (large images).
    FileGetOk {
        id: String,
        req_id: String,
        path: String,
        /// e.g. `image/png`, `image/jpeg`
        mime: String,
        size: u64,
        b64: String,
    },
    /// Client -> daemon: fetch the raw bytes of ANY file on the daemon
    /// machine so a client (mobile IDE) can download it to the device.
    /// This is the "save file to phone" path — unlike `FileGet` (restricted
    /// to registered image media) it is binary-safe and works on arbitrary
    /// files. Same trust boundary as `FileRead`/`FileWrite` (owner + machine,
    /// RLS-gated); capped at 16 MiB. The reply may arrive chunked when large.
    FileDownload {
        id: String,
        client: String,
        /// echoed back in `FileDownloadOk` / `error` so clients match the reply
        req_id: String,
        path: String,
    },
    /// Daemon -> client: the requested file's raw bytes, base64-encoded.
    /// May be chunked on the wire (large files).
    FileDownloadOk {
        id: String,
        req_id: String,
        path: String,
        /// best-effort MIME type guessed from the file extension
        mime: String,
        size: u64,
        b64: String,
    },
    /// Client -> daemon: move or rename a file/directory. `to` may be in the
    /// same directory (rename) or a different one (move). Refuses to clobber
    /// an existing destination or move a directory into its own subtree;
    /// cross-device moves fall back to copy+delete.
    FileMove {
        id: String,
        client: String,
        /// echoed back in `FileMoveOk` / `error` so clients match the reply
        req_id: String,
        /// existing source path (file or directory)
        from: String,
        /// destination path (must not already exist)
        to: String,
    },
    /// Daemon -> client: the move/rename succeeded; `path` is the new path.
    FileMoveOk {
        id: String,
        req_id: String,
        path: String,
    },
    /// Client -> daemon: delete a file or directory (directories are removed
    /// recursively). The client confirms with the user before sending.
    FileDelete {
        id: String,
        client: String,
        /// echoed back in `FileDeleteOk` / `error` so clients match the reply
        req_id: String,
        path: String,
    },
    /// Daemon -> client: the delete succeeded.
    FileDeleteOk {
        id: String,
        req_id: String,
        path: String,
    },
    /// Client -> daemon: copy a file/directory to a new location. The source
    /// is left in place. `to` must not already exist; copying a directory
    /// into its own subtree is refused.
    FileCopy {
        id: String,
        client: String,
        /// echoed back in `FileCopyOk` / `error` so clients match the reply
        req_id: String,
        /// existing source path (file or directory)
        from: String,
        /// destination path (must not already exist)
        to: String,
    },
    /// Daemon -> client: the copy succeeded; `path` is the new copy's path.
    FileCopyOk {
        id: String,
        req_id: String,
        path: String,
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
    /// Client -> daemon: list resumable local pi sessions on this machine.
    PiList {
        id: String,
        client: String,
        /// echoed back in PiListOk so clients match the reply
        req_id: String,
    },
    /// Daemon -> client: local pi sessions available for resume.
    PiListOk {
        id: String,
        req_id: String,
        sessions: Vec<PiSessionInfo>,
    },
    /// Client -> daemon: toggle whether to monitor all pi sessions on the system
    /// (including those started outside ranch).
    PiMonitor {
        enabled: bool,
        /// echoed back in PiMonitorOk so clients match the reply
        req_id: String,
    },
    /// Daemon -> client: ack the pi monitor toggle.
    PiMonitorOk {
        req_id: String,
        enabled: bool,
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
    /// Client -> daemon: hot-upgrade in place — exec the same binary with
    /// --inherit, passing all PTY/pi fds through (zero pane death).
    Upgrade {},
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
    ///
    /// `attachments` (optional) is a list of absolute file paths the
    /// user has attached. The daemon reads each file (capped at
    /// 50 KiB) and prepends its content to the prompt so the agent
    /// can see it immediately without a tool call.
    ChatSend {
        id: String,
        client: String,
        session: String,
        pane: String,
        text: String,
        #[serde(default)]
        attachments: Vec<String>,
    },
    /// Client -> daemon: interrupt the in-flight agent turn of a chat
    /// pane. Immediate and non-destructive: the session, conversation
    /// history, and (local-pi) agent process all survive. Local-pi
    /// panes get pi's `abort` RPC; forge-backed panes hit forge's
    /// `POST /sessions/{id}/interrupt`. A no-op when the agent is
    /// idle. Confirmation is out-of-band: a system chat row ("⏹ …")
    /// plus the usual `meta { kind: "agent", status: "idle" }`.
    Interrupt {
        id: String,
        client: String,
        session: String,
        pane: String,
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
    /// Client -> daemon: page back through a chat pane's history
    /// (mobile/web scrollback). Returns up to `limit` rows with
    /// `seq < before` (all rows when `before` is None), oldest first.
    /// `limit` is clamped to 200 server-side.
    ChatHistory {
        id: String,
        client: String,
        session: String,
        pane: String,
        /// echoed back in `ChatHistoryOk` so clients match the reply
        req_id: String,
        limit: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<i64>,
    },
    /// Daemon -> client: requested slice of a chat pane's history,
    /// oldest first. `has_more` = rows older than `msgs[0]` still exist.
    ChatHistoryOk {
        req_id: String,
        pane: String,
        msgs: Vec<ChatMsg>,
        #[serde(default)]
        has_more: bool,
    },
    /// Client -> daemon: list the agent models available to a chat pane
    /// plus the currently active one (pi: `get_available_models` +
    /// `get_state`; forge: the model catalog + session overrides).
    ModelList {
        id: String,
        client: String,
        /// chat pane id (uuid)
        pane: String,
        /// echoed back in `ModelListOk` so clients match the reply
        req_id: String,
    },
    /// Daemon -> client: model catalog for a chat pane. `current` is the
    /// active model (or None if unknown/legacy).
    ModelListOk {
        id: String,
        req_id: String,
        pane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<ModelChoice>,
        models: Vec<ModelChoice>,
    },
    /// Client -> daemon: switch the agent model of a chat pane.
    /// Success is confirmed out-of-band as `meta { kind: "model" }`
    /// with the new model's display name; failure → `error { req_id }`.
    ModelSet {
        id: String,
        client: String,
        session: String,
        pane: String,
        provider: String,
        model: String,
        /// echoed back on the `error` frame when the switch fails
        req_id: String,
    },
    /// Client -> daemon: manually compact the agent context of a chat
    /// pane (pi `compact` RPC for local-pi panes; forge's compact
    /// endpoint for forge-backed ones). Success is confirmed
    /// out-of-band as `meta { kind: "context" }` with the updated usage;
    /// failure → `error { req_id }`.
    ChatCompact {
        id: String,
        client: String,
        session: String,
        pane: String,
        /// echoed back on the `error` frame when the compact fails
        req_id: String,
    },
    /// Out-of-band status (no screen change).
    /// `kind` values: `"agent"` (working/idle), `"model"` (chat pane's
    /// active model reported or changed; `status` = display name),
    /// `"context"` (chat pane's context-window usage, e.g.
    /// `"31% · 62k/200k"`).
    Meta {
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// true = this idle transition closed a background turn (e.g.
        /// a mule wake) — clients should suppress the turn-end
        /// notification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background: Option<bool>,
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
    // ----- agent tools (Phase A: agents spawn/steer/close panes) -----
    /// Agent (or client) -> daemon: spawn a sub-agent pane.
    /// `mode` "split" = new pane in `caller_session`; "session" = new
    /// named session. `callback = true` registers a completion waiter
    /// (AgentDone on the spawned pane's working->idle transition).
    AgentSpawn {
        req_id: String,
        /// the pane requesting the spawn (ownership anchor; may be nil
        /// for human-initiated spawns)
        caller_pane: String,
        caller_session: String,
        kind: String, // "pi" | "forge"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default)]
        prompt: String,
        #[serde(default = "default_mode")]
        mode: String, // "split" | "session"
        #[serde(default)]
        callback: bool,
    },
    /// Daemon -> client: ack a successful spawn.
    AgentSpawnOk {
        req_id: String,
        spawn_id: String,
        session: String,
        pane: String,
    },
    /// Daemon -> clients (policy=ask): a spawn awaits approval.
    AgentSpawnRequest {
        spawn_id: String,
        caller_pane: String,
        kind: String,
        /// prompt preview (first ~120 chars)
        preview: String,
    },
    /// Client -> daemon: resolve a pending AgentSpawnRequest.
    AgentSpawnApprove {
        spawn_id: String,
        allow: bool,
    },
    /// Agent (or client) -> daemon: send a message to a spawned pane's
    /// agent. delivery: "steer" (pi RPC steer / forge message) or
    /// "queue" (pi follow_up / forge message).
    AgentSend {
        req_id: String,
        /// requesting pane (ownership anchor; nil = human)
        #[serde(default)]
        caller_pane: String,
        session: String,
        pane: String,
        text: String,
        #[serde(default = "default_delivery")]
        delivery: String,
    },
    /// Agent (or client) -> daemon: cheap status read of a spawned pane.
    AgentStatus {
        req_id: String,
        #[serde(default)]
        caller_pane: String,
        pane: String,
    },
    /// Daemon -> client: status reply.
    AgentStatusOk {
        req_id: String,
        pane: String,
        /// "working" | "idle" | "unknown"
        state: String,
        model: Option<String>,
    },
    /// Agent (or client) -> daemon: read recent conversation rows.
    AgentRead {
        req_id: String,
        #[serde(default)]
        caller_pane: String,
        pane: String,
        #[serde(default)]
        since_seq: i64,
        #[serde(default = "default_read_limit")]
        limit: u32,
    },
    /// Daemon -> client: conversation rows for AgentRead.
    AgentReadOk {
        req_id: String,
        pane: String,
        msgs: Vec<ChatMsg>,
    },
    /// Agent (or client) -> daemon: close a spawned pane (spawn-scoped).
    AgentClose {
        req_id: String,
        #[serde(default)]
        caller_pane: String,
        session: String,
        pane: String,
    },
    /// Daemon -> clients: ack (or callback) — a spawned pane finished
    /// (working->idle), was closed, was denied, or timed out.
    AgentDone {
        spawn_id: String,
        session: String,
        pane: String,
        /// "completed" | "failed" | "closed" | "denied" | "timeout"
        outcome: String,
        /// last assistant row (completed) or last row (failed)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_row: Option<ChatMsg>,
    },
    // ----- agent ask-user question (Phase A2) -----
    /// Agent (or client) -> daemon: ask the human a question and block
    /// the agent's tool call until it is answered. `choices` are the
    /// multiple-choice options (0..n); `suggested` is the agent's
    /// preferred index into `choices`; `multi` = select-many; `free_text`
    /// = also allow a blank user-typed answer.
    AgentAsk {
        req_id: String,
        /// requesting pane (ownership anchor; nil = human)
        #[serde(default)]
        caller_pane: String,
        #[serde(default)]
        question: String,
        #[serde(default)]
        choices: Vec<String>,
        /// suggested option (index into choices)
        suggested: Option<usize>,
        /// true = user may select many
        #[serde(default)]
        multi: bool,
        /// true = blank user-fillable option is offered
        #[serde(default = "default_true")]
        free_text: bool,
    },
    /// Daemon -> agent (control API reply): the question is pending.
    AgentAskOk {
        req_id: String,
        ask_id: String,
    },
    /// Daemon -> all clients (broadcast): a question awaits an answer.
    /// Clients render a prompt (TUI status bar, web/mobile card) and
    /// may push a local notification.
    AgentAskRequest {
        ask_id: String,
        session: String,
        pane: String,
        #[serde(default)]
        question: String,
        #[serde(default)]
        choices: Vec<String>,
        suggested: Option<usize>,
        #[serde(default)]
        multi: bool,
        #[serde(default = "default_true")]
        free_text: bool,
    },
    /// Client -> daemon: answer a pending AgentAskRequest. First
    /// answer wins; idempotent (later answers ignored).
    AgentAskAnswer {
        ask_id: String,
        /// selected choice indices (empty when only free text)
        #[serde(default)]
        choices: Vec<usize>,
        /// free-text answer (may be empty when a choice was picked)
        #[serde(default)]
        text: String,
    },
    /// Agent -> daemon (poll): has my question been answered yet?
    AgentAskStatus {
        req_id: String,
        #[serde(default)]
        caller_pane: String,
        ask_id: String,
    },
    /// Daemon -> agent: status reply. `answered` false until a client
    /// (or TTL expiry) resolves the ask.
    AgentAskStatusOk {
        req_id: String,
        ask_id: String,
        #[serde(default)]
        answered: bool,
        #[serde(default)]
        choices: Vec<usize>,
        #[serde(default)]
        text: String,
        /// "pending" | "answered" | "expired" | "unknown"
        state: String,
    },
    // ----- agent builder (Phase B): forge profile CRUD proxy -----
    /// Client -> daemon: the pi model catalog (for profile forms).
    /// Pane-less variant of `ModelList` (which needs a chat pane).
    ModelCatalog {
        req_id: String,
    },
    /// Daemon -> client: catalog entries (secrets stripped). Empty
    /// `models` when forge is unreachable — clients keep a free-text
    /// fallback so a custom model id still works.
    ModelCatalogOk {
        req_id: String,
        models: Vec<ModelChoice>,
    },
    /// Client -> daemon: list forge agent profiles.
    ProfileList {
        req_id: String,
    },
    /// Daemon -> client: profile summaries (secrets never included).
    ProfileListOk {
        req_id: String,
        profiles: Vec<ProfileSummary>,
    },
    /// Client -> daemon: fetch one profile (secret redacted by forge).
    ProfileGet {
        req_id: String,
        profile: String,
    },
    /// Daemon -> client: the profile.
    ProfileGetOk {
        req_id: String,
        profile: Profile,
    },
    /// Client -> daemon: create (profile_id absent) or update. `api_key`
    /// is write-only: forge accepts it and never returns it.
    ProfilePut {
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        draft: ProfileDraft,
    },
    /// Daemon -> client: the saved profile's id.
    ProfilePutOk {
        req_id: String,
        profile_id: String,
    },
    /// Client -> daemon: delete a profile.
    ProfileDelete {
        req_id: String,
        profile: String,
    },
    /// Daemon -> client: delete ack.
    ProfileDeleteOk {
        req_id: String,
    },
    // ----- workflows (Phase C): mule proxy -----
    /// Client -> daemon: list mule workflows.
    WorkflowList {
        req_id: String,
    },
    /// Client -> daemon: mule agents (for workflow-step pickers).
    MuleAgents {
        req_id: String,
    },
    /// Daemon -> client: agent summaries (id + name).
    MuleAgentsOk {
        req_id: String,
        agents: Vec<MuleAgent>,
    },
    /// Daemon -> client: workflow summaries.
    WorkflowListOk {
        req_id: String,
        workflows: Vec<WorkflowSummary>,
    },
    /// Client -> daemon: fetch one workflow (with steps).
    WorkflowGet {
        req_id: String,
        workflow: String,
    },
    /// Daemon -> client: the workflow + steps.
    WorkflowGetOk {
        req_id: String,
        workflow: WorkflowSummary,
        steps: Vec<WorkflowStep>,
    },
    /// Client -> daemon: create (workflow_id absent) or update a
    /// workflow with its full step list (the worker diffs mule's
    /// step endpoints).
    WorkflowPut {
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow_id: Option<String>,
        draft: WorkflowDraft,
    },
    /// Daemon -> client: the saved workflow's id.
    WorkflowPutOk {
        req_id: String,
        workflow_id: String,
    },
    /// Client -> daemon: delete a workflow.
    WorkflowDelete {
        req_id: String,
        workflow: String,
    },
    /// Daemon -> client: delete ack.
    WorkflowDeleteOk {
        req_id: String,
    },
    /// Client -> daemon: run a workflow into a new pane.
    WorkflowRun {
        req_id: String,
        workflow: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    /// Daemon -> client: the run started (pane carries the stream).
    WorkflowRunOk {
        req_id: String,
        job: String,
        session: String,
        pane: String,
    },
    // ----- agent registry (Herd H1): forge `agents` rows ----- //
    /// Client -> daemon: list the agents registered in forge (the
    /// "herd"). Secrets/credentials never cross the wire.
    AgentList {
        req_id: String,
    },
    /// Daemon -> client: the agent registry.
    AgentListOk {
        req_id: String,
        agents: Vec<AgentInfo>,
    },
    /// Client -> daemon: open a conversation with an agent and attach a
    /// pane to it. `mode`: "continue" = the agent's most-active
    /// conversation (a fresh one is created when it has none);
    /// "new" = a fresh conversation; "fork" = a sibling forked from
    /// `fork_from` (or the most-active conversation when absent).
    /// When `text` is present the daemon sends it as the first message
    /// of the attached conversation.
    AgentTalk {
        req_id: String,
        /// agent id (uuid string)
        agent: String,
        /// "continue" | "new" | "fork"
        mode: String,
        /// first message, sent immediately on the new/attached pane
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// conversation title (new/fork; continue keeps its title)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// fork point for mode="fork" (a conversation of the same agent)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork_from: Option<String>,
    },
    /// Daemon -> client: the conversation pane is ready.
    /// `session`/`pane` are the new ranch session + chat pane.
    AgentTalkOk {
        req_id: String,
        session: String,
        pane: String,
    },
    /// Client -> daemon: is this agent busy right now, and which
    /// conversation is it in? (Named AgentBusy: `AgentStatus` is taken
    /// by the Phase A pane-level frame.)
    AgentBusy {
        req_id: String,
        agent: String,
    },
    /// Daemon -> client: the busy flag + the agent's most-active
    /// conversation (None when the agent has no conversations).
    AgentBusyOk {
        req_id: String,
        busy: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_conversation: Option<String>,
    },
    /// Client -> daemon: pull an agent's recent activity (H5's
    /// Activity View data). H1 ships the frame only — the daemon
    /// always answers an empty list until H5 lands the source.
    AgentActivity {
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// activity kinds to include (H5 taxonomy; empty = all)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kinds: Option<Vec<String>>,
    },
    /// Daemon -> client: activity items, most recent first
    /// (H1: always empty; H5 fills it).
    AgentActivityOk {
        req_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<ActivityItem>,
    },
    /// Daemon -> client: push the FULL Activity View item set (H5.3).
    /// The daemon's activity poller recomputes the set every 5 s (local
    /// asks/spawns/trigger-fires/mule jobs + per-agent forge feeds +
    /// the mule wake log) and broadcasts it only when the set changes
    /// (2 s debounce coalesces rapid churn). Clients replace their
    /// timeline with `items` (most recent first; an item's identity
    /// key is (kind, agent, id)). Additive: older clients drop the
    /// unknown tag; the H1 `AgentActivity` request path stays as-is.
    ActivityPush {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<ActivityItem>,
    },
    /// Client -> daemon: pause (or resume) one agent, or the whole
    /// herd when `agent` is None (H5.3 global kill switch). The
    /// daemon calls forge `POST /agents/:id/pause|resume` (idempotent)
    /// and mule `POST /api/v1/wakes/pause` (best-effort: no mule
    /// config => that leg is simply not attempted). Pending asks stay
    /// visible while an agent is paused — pause halts new work, not
    /// observability.
    AgentPause {
        req_id: String,
        /// agent id or registry name; None = every known agent
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// false = resume
        paused: bool,
    },
    /// Daemon -> client: per-agent pause/resume results.
    AgentPauseOk {
        req_id: String,
        results: Vec<AgentPauseResult>,
    },
    // ----- H6.5 specialist-dot provisioning (client -> daemon) -----
    /// Client -> daemon: provision a new agent (the `ranch agents new`
    /// wizard, mobile-flavored). The daemon runs the same core the CLI
    /// uses: POST /agents (forge) + optional POST /api/v1/wakes
    /// (mule cron wake). The interactive prompts don't exist here —
    /// the mobile form collects the fields.
    AgentProvision {
        req_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        /// "agent" | "org" (forge ALLOWED_MEMORY_SCOPES)
        memory_scope: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        org_id: Option<String>,
        home_machine: String,
        #[serde(default)]
        tools_allowlist: Vec<String>,
        /// 5-field cron expr ("07:00" is converted client-side)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_cron: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_prompt: Option<String>,
    },
    /// Daemon -> client: the agent row landed (+ optional wake).
    AgentProvisionOk {
        req_id: String,
        agent_id: String,
        agent_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_error: Option<String>,
    },
    /// Client -> daemon: delete a herd agent (forge DELETE /agents/:id).
    AgentDelete {
        req_id: String,
        agent_id: String,
    },
    /// Daemon -> client: the agent was deleted.
    AgentDeleteOk {
        req_id: String,
    },
    /// Client -> daemon: edit a herd agent (the mobile ✎ form / `ranch
    /// agents edit`). The daemon PATCHes forge `/agents/:id`. Every
    /// field is optional and **absent means "leave the column
    /// alone"** — forge's partial update treats JSON `null` and an absent key the
    /// same, so clearing a nullable column (avatar_url, home_machine,
    /// primary_profile_id, extra_instructions) is NOT supported (forge
    /// H1 limitation, documented).
    AgentUpdate {
        req_id: String,
        agent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        /// "agent" | "org" (forge ALLOWED_MEMORY_SCOPES)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory_scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        org_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        home_machine: Option<String>,
        /// "private" | "org" (forge ALLOWED_VISIBILITIES)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visibility: Option<String>,
        /// Some(vec![]) = allow all (the H1.1 empty-allowlist convention)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools_allowlist: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra_instructions: Option<String>,
    },
    /// Daemon -> client: the edited agent (the fresh forge row).
    AgentUpdateOk {
        req_id: String,
        agent: AgentInfo,
    },
    // ----- H6.5b wake editing (client -> daemon, mule proxy) -----
    /// Client -> daemon: list the mule wakes bound to one agent (the
    /// editor's schedule section). The daemon GETs
    /// `{mule}/api/v1/wakes` and filters by `target.agent_id`.
    WakeList {
        req_id: String,
        agent_id: String,
    },
    /// Daemon -> client: the agent's wakes (cron and event kinds).
    WakeListOk {
        req_id: String,
        wakes: Vec<WakeInfo>,
    },
    /// Client -> daemon: create (wake_id None) or update (wake_id Some)
    /// one of the agent's wakes. Creating uses the same H3.3
    /// `target = {forge_conversation: {forge_url, api_key, agent_id}}`
    /// shape the provisioning core uses.
    WakePut {
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_id: Option<String>,
        agent_id: String,
        /// 5-field cron expr ("07:00" is converted client-side)
        cron: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// false = paused (None on create = enabled)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
    },
    /// Daemon -> client: the wake's id.
    WakePutOk {
        req_id: String,
        wake_id: String,
    },
    /// Client -> daemon: delete one wake.
    WakeDelete {
        req_id: String,
        wake_id: String,
    },
    /// Daemon -> client: the wake was deleted.
    WakeDeleteOk {
        req_id: String,
    },
    // ----- sandbox package governance (client -> daemon, forge proxy) -----
    /// Client -> daemon: the agent's sandbox package state (approved /
    /// pending / rejected + base readiness + approver identity). The
    /// daemon GETs `{forge}/agents/{id}/sandbox/packages`; the mobile
    /// agent editor renders the queue and its approve/reject buttons
    /// (2026-10-18).
    SandboxPackages {
        req_id: String,
        agent_id: String,
    },
    /// Daemon -> client: forge's JSON payload verbatim
    /// (`base_ready`, `approver`, `installed|approved|pending|rejected`).
    SandboxPackagesOk {
        req_id: String,
        data: Value,
    },
    /// Client -> daemon: decide one pending request (the phone's
    /// approve/reject tap; the daemon calls forge as the USER key, so
    /// the decision is `decided_by: "user"`).
    SandboxDecide {
        req_id: String,
        request_id: String,
        approve: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Daemon -> client: decision result (`{ok, status, package}`).
    SandboxDecideOk {
        req_id: String,
        data: Value,
    },
    /// Client -> daemon: wipe the agent's sandbox rootfs trees (the
    /// editor's "reinstall" — working files are kept; approved
    /// packages re-install automatically).
    SandboxReset {
        req_id: String,
        agent_id: String,
    },
    /// Client -> daemon: the USER asks for packages on an agent's
    /// behalf (the editor's ＋ button). Lands as a normal pending
    /// request (`requested_by: "user"`) — still EA-approvable, so the
    /// audit chain is identical.
    SandboxRequest {
        req_id: String,
        agent_id: String,
        packages: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Daemon -> client: `{requested:[{id,package,status}], skipped:
    /// [{package,why}]}` verbatim from forge.
    SandboxRequestOk {
        req_id: String,
        data: Value,
    },
    /// Daemon -> client: how many rootfs trees were removed.
    SandboxResetOk {
        req_id: String,
        rootfs_removed: u64,
    },
    /// Daemon -> clients: an explicit user notification emitted by an
    /// agent (its `notify_user` tool / `@notify:` message prefix).
    /// Clients post this as a notification regardless of background
    /// suppression; `priority` is "high" | "normal" | "quiet".
    AgentNotify {
        session: String,
        pane: String,
        agent: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<String>,
    },
    // ----- triggers (Phase D) -----
    /// Client -> daemon: list trigger definitions + last runs.
    TriggerList {
        req_id: String,
    },
    /// Daemon -> client: trigger list.
    TriggerListOk {
        req_id: String,
        triggers: Vec<serde_json::Value>,
    },
    /// Client -> daemon: create (trigger_id absent) or update.
    TriggerPut {
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger_id: Option<String>,
        /// full trigger shape (SPEC §6.2): name, workflow, kind, cron|
        /// event|webhook spec, input, enabled, catch_up
        trigger: serde_json::Value,
    },
    /// Daemon -> client: the saved trigger's id.
    TriggerPutOk {
        req_id: String,
        trigger_id: String,
    },
    /// Client -> daemon: delete a trigger.
    TriggerDelete {
        req_id: String,
        trigger: String,
    },
    /// Daemon -> client: delete ack.
    TriggerDeleteOk {
        req_id: String,
    },
    /// Client -> daemon: fire a trigger immediately (manual run).
    TriggerRun {
        req_id: String,
        trigger: String,
    },
    /// Daemon -> clients: a trigger fired (dashboard badge / toast).
    TriggerFired {
        trigger: String,
        job: String,
    },
    // ----- webhooks (Phase E) -----
    /// Client -> daemon: list webhooks (secrets never returned).
    WebhookList {
        req_id: String,
    },
    /// Daemon -> client: webhook list.
    WebhookListOk {
        req_id: String,
        webhooks: Vec<serde_json::Value>,
    },
    /// Client -> daemon: create a webhook. The reply carries the URL +
    /// raw secret exactly once (stored encrypted server-side).
    WebhookPut {
        req_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        webhook_id: Option<String>,
        name: String,
        /// allowed source tags (empty = any)
        #[serde(default)]
        sources: Vec<String>,
    },
    /// Daemon -> client: webhook saved; secret/url shown once.
    WebhookPutOk {
        req_id: String,
        webhook_id: String,
        url: String,
        /// raw secret — ONLY present on creation
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },
    /// Client -> daemon: delete a webhook.
    WebhookDelete {
        req_id: String,
        webhook: String,
    },
    /// Daemon -> client: delete ack.
    WebhookDeleteOk {
        req_id: String,
    },
    /// Edge function -> daemon (via the relay): a verified external
    /// event. The daemon matches it against webhook triggers and fires
    /// workflows. This frame can only arrive from the relay pipe.
    WebhookEvent {
        webhook: String,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event: Option<String>,
        #[serde(default)]
        payload: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        received_at: Option<String>,
    },
}

fn default_mode() -> String {
    "split".into()
}
fn default_delivery() -> String {
    "steer".into()
}
fn default_read_limit() -> u32 {
    50
}
fn default_true() -> bool {
    true
}

/// One row of the agent registry (Herd H1). An "agent" is a
/// first-class forge `agents` row — distinct from a profile (the
/// model/tool config an agent runs on) and a session (a conversation
/// the agent has had).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentInfo {
    /// agent id (uuid string)
    pub id: String,
    /// registry name (unique per owner in forge)
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// ranch machine id this agent "lives on" (None = unbound).
    /// Ranch refuses to open a pane when this is another machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_machine: Option<String>,
    /// most-recent activity across the agent's conversations
    /// (ISO-8601). H1 fills None (TODO(H5): derive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active: Option<String>,
    /// how many conversations the agent has (H1: None; TODO(H5)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_count: Option<u64>,
    /// the agent's model profile (forge `agents.primary_profile_id`);
    /// None = no model attached. Filled from `GET /agents` (H6.5b —
    /// the editor prefills its profile chip from it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_profile: Option<String>,
    /// "agent" | "org" (forge `agents.memory_scope`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_scope: Option<String>,
    /// declared memory org label (H6.5), when the scope is "org".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    /// "private" | "org" (forge `agents.visibility`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// the agent's tool allowlist; Some(vec![]) = allow all. None =
    /// forge did not return the column (older server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_allowlist: Option<Vec<String>>,
    /// free-form instructions appended to the agent's system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_instructions: Option<String>,
    /// the H5.3 kill-switch state (forge `agents.paused`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

/// One row of the agent's mule wake list (H6.5b — the editor's
/// schedule section). Mirrors mule's `wakeAPI` (`cmd/api/
/// wake_handlers.go`) for the fields the editor shows; `spec.expr` is
/// surfaced as `cron` for cron wakes (None for event/other kinds).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WakeInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// "cron" | "event" | "interval" | … (mule wake kinds)
    #[serde(default)]
    pub kind: String,
    /// the cron expression for cron wakes (None otherwise)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_at: Option<String>,
}

/// One row of the Activity View (H5). H1 ships the frame only;
/// H5.3 fills it from the daemon's activity poller (push via
/// `ActivityPush`, request via `AgentActivity`/`AgentActivityOk`):
/// `kind` ∈ "ask" (pending approval), "spawn" (agent-spawned pane /
/// awaiting approval), "trigger" (recent trigger fire, 10-min
/// window), "mule_job" (recent machine-launched mule job, 10-min
/// window), "forge_task" (durable task, non-terminal),
/// "belief_pending" (pending belief), "research_open" (open research
/// task), "wake" (mule wake_log entry). `text` is a one-line summary,
/// truncated to 200 chars; `ts` is RFC 3339 (UTC `Z`) — sources
/// without a timestamp (e.g. durable tasks) use the poll time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityItem {
    /// stable item id (H5 taxonomy)
    pub id: String,
    /// "message" | "task" | "approval" | "research" | ... (H5)
    pub kind: String,
    /// the agent the item is about (None = machine-wide)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// one-line summary
    pub text: String,
    /// ISO-8601 timestamp
    pub ts: String,
}

/// One agent's pause/resume outcome (H5.3). `forge` = the agent's
/// resulting paused state (None = not attempted: forge unconfigured);
/// `mule` = how many of the agent's wakes were flipped (None = not
/// attempted: mule unconfigured); `error` = failure detail naming the
/// leg that failed (the other leg may still have succeeded).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentPauseResult {
    /// agent id (uuid string)
    pub agent: String,
    /// registry name (the id itself when unknown)
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mule: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    /// Tool call arguments as compact JSON (pi `args` / forge
    /// `tool_input`) — lets clients show the command, not just the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// File paths attached to this message (user role only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<String>>,
    /// Absolute paths of images this row references — user rows that
    /// attached an image, and tool rows whose call/result read an image
    /// (e.g. the agent's `Read` tool on a png). All values are media
    /// registry paths, so any client can fetch the bytes with `FileGet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_refs: Option<Vec<String>>,
}

/// One window in a session's window stack (M5). A window is a named
/// layout tree; exactly one is active per session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowSnap {
    pub id: String,
    pub name: String,
    pub layout: Layout,
}

/// A resumable local pi session (PiListOk row).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PiSessionInfo {
    /// The pane id that owns this pi session (used to attach).
    /// For external sessions this is the session UUID from the .jsonl file.
    pub id: String,
    /// Human-readable title (last user message of the session, truncated).
    /// Falls back to the working directory if the session has no messages yet.
    #[serde(default)]
    pub title: String,
    /// The pi session file path on disk.
    pub session_file: String,
    /// Working directory of the session.
    #[serde(default)]
    pub path: String,
    /// Whether the pi process is currently alive.
    pub active: bool,
    /// True if this session was started outside ranch (external pi session).
    #[serde(default)]
    pub external: bool,
    /// Last-modified timestamp (ISO 8601) for sorting.
    #[serde(default)]
    pub updated: String,
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
    /// The session's working directory, when forge has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// Forge agent-profile summary (agent builder, Phase B). Secrets are
/// redacted server-side; these rows are safe to render anywhere.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Full forge profile (ProfileGet). `api_key` arrives redacted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nix_shell: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Create/update payload (ProfilePut.draft). Mirrors forge's
/// `CreateProfile`; `api_key` is write-only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileDraft {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nix_shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Mule agent summary (workflow-step picker rows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MuleAgent {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Mule workflow summary (list rows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowSummary {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_async: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// One workflow step (mule WorkflowStep shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub step_order: i64,
    /// "agent" | "wasm_module"
    #[serde(rename = "type", default)]
    pub step_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm_module_id: Option<String>,
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Create/update payload: the workflow + the full desired step list
/// (the mule worker diffs against what's stored).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowDraft {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub is_async: bool,
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
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
    /// Session working directory — the active pane's cwd for PTY panes
    /// (live /proc read), the recorded spawn dir for chat panes. Absent
    /// from older daemons / when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// True when any agent pane in the session is mid-turn. Absent from
    /// older daemons (deserializes as false).
    #[serde(default)]
    pub busy: bool,
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
    /// For chat panes: display name of the agent's active model
    /// (filled when the daemon knows: pi `get_state`, forge
    /// session/profile lookup, or after a model switch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// For chat panes: latest context-window usage readout
    /// (cached from `meta kind="context"` broadcasts; carried on the
    /// snapshot so attaching clients see it without waiting for a
    /// turn to end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// For chat panes when the attaching client set `chat_limit`: true
    /// when `chat` was truncated and older rows exist (page back via
    /// `ChatHistory`). Absent when the full history was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_has_more: Option<bool>,
    /// PTY panes only: current working directory of the child
    /// (/proc/<pid>/cwd), for the file browser's start dir and the
    /// `$EDITOR` split anchor. None for chat panes / unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// One selectable agent model (pi `models.json` entry or forge
/// catalog row). `id` is the model identifier; `name` is the
/// human-friendly display name (falls back to `id`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelChoice {
    pub provider: String,
    pub id: String,
    pub name: String,
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
    Leaf {
        pane: String,
    },
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
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
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
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for i in (0..s.len()).step_by(4) {
        let a = b64_val(s[i])?;
        let b = b64_val(s[i + 1])?;
        let c = if s[i + 2] == b'=' {
            0
        } else {
            b64_val(s[i + 2])?
        };
        let d = if s[i + 3] == b'=' {
            0
        } else {
            b64_val(s[i + 3])?
        };
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
/// Measured 2026-09-27: Supabase Realtime's private-channel broadcast
/// silently drops frames above ~256 KiB (a 256,000 B frame arrives, a
/// 266,240 B one never does). 192 KiB keeps a chunked broadcast —
/// envelope included — well under that cap.
pub const MAX_FRAME: usize = 192 * 1024;

/// Largest chunk batch a reassembler will accept. Bounds the reassembly
/// allocation (512 × 192 KiB ≈ 96 MiB worst case) and makes hostile or
/// corrupt batches (`n` arrives over the wire) cheap to drop. The largest
/// legitimate payload is the 10 MiB FilePut cap (~70 chunks).
pub const MAX_CHUNKS: u32 = 512;

/// Serialize a frame into one or more JSON-lines. If the serialized frame is
/// under `MAX_FRAME`, returns a single line. Otherwise returns `n` chunk
/// frames whose `data` fields reassemble to the original JSON. Splits on
/// char boundaries so multi-byte UTF-8 is never mangled.
pub fn encode_frame(frame: &Frame, chunk_id: &str) -> Vec<String> {
    let json = serde_json::to_string(frame).expect("frame serializes");
    if json.len() <= MAX_FRAME {
        return vec![json];
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut start = 0;
    while start < json.len() {
        let mut end = json.len().min(start + MAX_FRAME);
        while !json.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(json[start..end].to_string());
        start = end;
    }
    let n = pieces.len() as u32;
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, piece)| {
            serde_json::to_string(&Frame::Chunk {
                chunk_id: chunk_id.to_string(),
                i: i as u32,
                n,
                data: piece,
            })
            .expect("chunk serializes")
        })
        .collect()
}

/// Incremental frame decoder. Feed it raw bytes (which may contain zero, one,
/// or many partial/complete JSON-lines, interleaved with chunk frames). It
/// reassembles chunked frames and yields complete `Frame`s.
pub struct Decoder {
    buf: Vec<u8>,
    pending: HashMap<String, (u32, Vec<Option<String>>)>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pending: HashMap::new(),
        }
    }

    /// Feed a chunk of bytes; returns any complete frames decoded from it.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        self.buf.extend_from_slice(bytes);
        // fast path: after any feed the buffer holds no newline, so one
        // exists only if the new bytes carry one — no rescan needed
        if !bytes.contains(&b'\n') {
            return frames;
        }
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // drop the \n
            let line = std::str::from_utf8(&line).unwrap_or("");
            if line.is_empty() {
                continue;
            }
            if let Ok(f) = serde_json::from_str::<Frame>(line)
                && let Some(frame) = self.maybe_complete(&f)
            {
                frames.push(frame);
            }
        }
        frames
    }

    /// If `f` is the last chunk of a pending frame, reassemble and return it.
    fn maybe_complete(&mut self, f: &Frame) -> Option<Frame> {
        match f {
            Frame::Chunk {
                chunk_id,
                i,
                n,
                data,
            } => {
                // corrupt/hostile batch: zero/absurd `n` or out-of-range
                // index — drop it without allocating the slot vector
                if *n == 0 || *i >= *n || *n > MAX_CHUNKS {
                    return None;
                }
                let complete = {
                    let entry = self
                        .pending
                        .entry(chunk_id.clone())
                        .or_insert_with(|| (*n, vec![None; *n as usize]));
                    let slots = &mut entry.1;
                    if (*i as usize) < slots.len() && slots[*i as usize].is_none() {
                        slots[*i as usize] = Some(data.clone());
                    }
                    slots.iter().all(|s| s.is_some())
                };
                if complete {
                    let joined: String = self
                        .pending
                        .get(chunk_id)
                        .map(|(_, slots)| {
                            slots.iter().map(|s| s.as_deref().unwrap_or("")).collect()
                        })
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
            chat_limit: None,
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
        // > MAX_FRAME (192 KiB) so this actually splits
        let big = "x".repeat(200_000);
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

    /// Non-ASCII content straddling the MAX_FRAME boundary must survive
    /// the split: byte-aligned slicing would replace the boundary char
    /// with U+FFFD and corrupt the reassembled frame.
    #[test]
    fn encode_frame_char_boundary_utf8() {
        // "あ" is 3 bytes; pad with ASCII so the 192 KiB cut lands inside
        // a run of multi-byte chars
        let filler = "あ".repeat(70_000); // 210 KB in UTF-8
        let f = Frame::FilePut {
            id: "i1".into(),
            client: "c".into(),
            req_id: "fp-1".into(),
            name: filler.clone(),
            b64: "QUJD".into(),
        };
        let lines = encode_frame(&f, "c-utf8");
        assert!(lines.len() > 1, "payload should split");
        // every piece must be valid UTF-8 on its own (serde_json requires
        // it; from_utf8_lossy corruption would also show up here as the
        // replacement char)
        for l in &lines {
            let v = serde_json::from_str::<serde_json::Value>(l).unwrap();
            let data = v.get("data").unwrap().as_str().unwrap();
            assert!(!data.contains('\u{FFFD}'), "mangled chunk");
        }
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for l in &lines {
            let mut payload = l.clone();
            payload.push('\n');
            got.extend(d.feed(payload.as_bytes()));
        }
        assert_eq!(got, vec![f], "reassembled frame must be byte-identical");
    }

    /// A chunk batch whose `n` exceeds MAX_CHUNKS must be dropped, not
    /// allocated (a hostile n=4 billion would request a 32 GB vec).
    #[test]
    fn decoder_rejects_hostile_chunk_n() {
        let mut d = Decoder::new();
        let hostile = serde_json::json!({"t":"Chunk","chunk_id":"evil","i":0,"n":4_000_000_000u32,"data":"x"});
        let mut line = hostile.to_string();
        line.push('\n');
        let got = d.feed(line.as_bytes());
        assert!(got.is_empty(), "hostile batch must not reassemble");
        // and a normal small batch still works afterwards
        let f = Frame::Hb;
        let mut got2 = Vec::new();
        for l in encode_frame(&f, "ok") {
            let mut payload = l;
            payload.push('\n');
            got2.extend(d.feed(payload.as_bytes()));
        }
        assert_eq!(got2, vec![f]);
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
                kind: None,
                chat: None,
                forge_session: None,
                model: None,
                context: None,
                chat_has_more: None,
                cwd: None,
            }],
            meta: vec![],
            windows: vec![],
            window: None,
        };
        // Serialize everything into one byte blob, feed in 4 KiB blocks
        // (exercises partial lines and interleaved chunks).
        let blob: Vec<u8> = encode_frame(&f, "c9").join("\n").into_bytes();
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for chunk in blob.chunks(4096) {
            got.extend(d.feed(chunk));
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
    fn roundtrip_file_frames() {
        let read = Frame::FileRead {
            id: "i1".into(),
            client: "c".into(),
            req_id: "r1".into(),
            path: "/home/j/notes.md".into(),
        };
        let read_ok = Frame::FileReadOk {
            id: "i2".into(),
            req_id: "r1".into(),
            path: "/home/j/notes.md".into(),
            content: "# hi\n".into(),
            mtime: 1757500000,
            size: 7,
        };
        let write = Frame::FileWrite {
            id: "i3".into(),
            client: "c".into(),
            req_id: "r2".into(),
            path: "/home/j/notes.md".into(),
            content: "# hi there\n".into(),
            mtime: Some(1757500000),
        };
        let write_ok = Frame::FileWriteOk {
            id: "i4".into(),
            req_id: "r2".into(),
            path: "/home/j/notes.md".into(),
            mtime: 1757500001,
        };
        let changed = Frame::FileChanged {
            path: "/home/j/notes.md".into(),
            mtime: 1757500002,
        };
        let put = Frame::FilePut {
            id: "i5".into(),
            client: "mobile".into(),
            req_id: "r3".into(),
            name: "notes.txt".into(),
            b64: "aGVsbG8=".into(),
        };
        let put_ok = Frame::FilePutOk {
            id: "i6".into(),
            req_id: "r3".into(),
            path: "/home/j/.local/state/ranch/uploads/1757500002-notes.txt".into(),
            size: 5,
        };
        let get = Frame::FileGet {
            id: "i7".into(),
            client: "web".into(),
            req_id: "r4".into(),
            path: "/home/j/.local/state/ranch/uploads/1757500003-s.png".into(),
        };
        let get_ok = Frame::FileGetOk {
            id: "i8".into(),
            req_id: "r4".into(),
            path: "/home/j/.local/state/ranch/uploads/1757500003-s.png".into(),
            mime: "image/png".into(),
            size: 4,
            b64: "iVBORw0KGgo=".into(),
        };
        let download = Frame::FileDownload {
            id: "i9".into(),
            client: "mobile".into(),
            req_id: "r5".into(),
            path: "/home/j/config.toml".into(),
        };
        let download_ok = Frame::FileDownloadOk {
            id: "i10".into(),
            req_id: "r5".into(),
            path: "/home/j/config.toml".into(),
            mime: "application/toml".into(),
            size: 12,
            b64: "a2V5ID0gInYiCg==".into(),
        };
        let mv = Frame::FileMove {
            id: "i11".into(),
            client: "mobile".into(),
            req_id: "r6".into(),
            from: "/home/j/a.txt".into(),
            to: "/home/j/b/a.txt".into(),
        };
        let mv_ok = Frame::FileMoveOk {
            id: "i12".into(),
            req_id: "r6".into(),
            path: "/home/j/b/a.txt".into(),
        };
        let del = Frame::FileDelete {
            id: "i13".into(),
            client: "mobile".into(),
            req_id: "r7".into(),
            path: "/home/j/a.txt".into(),
        };
        let del_ok = Frame::FileDeleteOk {
            id: "i14".into(),
            req_id: "r7".into(),
            path: "/home/j/a.txt".into(),
        };
        let cp = Frame::FileCopy {
            id: "i15".into(),
            client: "mobile".into(),
            req_id: "r8".into(),
            from: "/home/j/a.txt".into(),
            to: "/home/j/copy-of-a.txt".into(),
        };
        let cp_ok = Frame::FileCopyOk {
            id: "i16".into(),
            req_id: "r8".into(),
            path: "/home/j/copy-of-a.txt".into(),
        };
        for f in [
            put,
            put_ok.clone(),
            get,
            get_ok.clone(),
            download,
            download_ok,
            mv,
            mv_ok,
            del,
            del_ok,
            cp,
            cp_ok,
        ] {
            let line = encode_frame(&f, "c");
            assert_eq!(line.len(), 1);
            let back: Frame = serde_json::from_str(&line[0]).unwrap();
            assert_eq!(back, f);
        }
        // DirList.hidden defaults to false when the client omits it
        let dl = Frame::DirList {
            id: "i".into(),
            client: "c".into(),
            req_id: "r".into(),
            path: Some("/home/j".into()),
            hidden: true,
        };
        let line = encode_frame(&dl, "c");
        let back: Frame = serde_json::from_str(&line[0]).unwrap();
        match back {
            Frame::DirList { hidden, .. } => assert!(hidden),
            _ => panic!("wrong variant"),
        }
        let absent: Frame = serde_json::from_str(
            r#"{"t":"DirList","id":"i","client":"c","req_id":"r","path":"/x"}"#,
        )
        .unwrap();
        match absent {
            Frame::DirList { hidden, .. } => assert!(!hidden),
            _ => panic!("wrong variant"),
        }
        // optional mtime must deserialize as None when absent (back-compat)
        let json =
            r#"{"t":"FileWrite","id":"i","client":"c","req_id":"r","path":"/x","content":""}"#;
        let parsed: Frame = serde_json::from_str(json).unwrap();
        match parsed {
            Frame::FileWrite { mtime, .. } => assert!(mtime.is_none()),
            _ => panic!("wrong variant"),
        }
        for f in [read, read_ok, write, write_ok, changed] {
            let lines = encode_frame(&f, "c1");
            let mut payload = String::new();
            for l in &lines {
                payload.push_str(l);
                payload.push('\n');
            }
            let mut d = Decoder::new();
            let got = d.feed(payload.as_bytes());
            assert_eq!(got, vec![f]);
        }
    }

    #[test]
    fn decoder_handles_interleaved_lines() {
        let f1 = Frame::Hb;
        let f2 = Frame::SessionsKill {
            session: "s".into(),
        };
        let mut payload = String::new();
        payload.push_str(&serde_json::to_string(&f1).unwrap());
        payload.push('\n');
        payload.push_str(&serde_json::to_string(&f2).unwrap());
        payload.push('\n');
        let mut d = Decoder::new();
        let frames = d.feed(payload.as_bytes());
        assert_eq!(frames, vec![f1, f2]);
    }

    // ----- agent tool frames (Phase A) -----

    #[test]
    fn agent_frames_roundtrip() {
        let frames = vec![
            Frame::AgentSpawn {
                req_id: "r1".into(),
                caller_pane: "00000000-0000-0000-0000-000000000000".into(),
                caller_session: "sess".into(),
                kind: "pi".into(),
                profile_id: None,
                name: Some("refactor".into()),
                cwd: Some("/tmp".into()),
                prompt: "do the thing".into(),
                mode: "split".into(),
                callback: true,
            },
            Frame::AgentSpawnOk {
                req_id: "r1".into(),
                spawn_id: "sp1".into(),
                session: "sess".into(),
                pane: "pane".into(),
            },
            Frame::AgentSpawnRequest {
                spawn_id: "sp2".into(),
                caller_pane: "pane-a".into(),
                kind: "forge".into(),
                preview: "refactor the widget modu...".into(),
            },
            Frame::AgentSpawnApprove {
                spawn_id: "sp2".into(),
                allow: true,
            },
            Frame::AgentSend {
                req_id: "r2".into(),
                caller_pane: "pane-a".into(),
                session: "sess".into(),
                pane: "pane".into(),
                text: "stop, do this instead".into(),
                delivery: "steer".into(),
            },
            Frame::AgentStatus {
                req_id: "r3".into(),
                caller_pane: "pane-a".into(),
                pane: "pane".into(),
            },
            Frame::AgentStatusOk {
                req_id: "r3".into(),
                pane: "pane".into(),
                state: "working".into(),
                model: Some("claude".into()),
            },
            Frame::AgentRead {
                req_id: "r4".into(),
                caller_pane: "pane-a".into(),
                pane: "pane".into(),
                since_seq: 5,
                limit: 50,
            },
            Frame::AgentReadOk {
                req_id: "r4".into(),
                pane: "pane".into(),
                msgs: vec![ChatMsg {
                    seq: 6,
                    role: "assistant".into(),
                    text: "done".into(),
                    tool_name: None,
                    tool_call_id: None,
                    tool_output: None,
                    tool_args: None,
                    duration_ms: None,
                    created_at: None,
                    attachments: None,
                    image_refs: None,
                }],
            },
            Frame::AgentClose {
                req_id: "r5".into(),
                caller_pane: "pane-a".into(),
                session: "sess".into(),
                pane: "pane".into(),
            },
            Frame::AgentDone {
                spawn_id: "sp1".into(),
                session: "sess".into(),
                pane: "pane".into(),
                outcome: "completed".into(),
                last_row: None,
            },
            Frame::AgentAsk {
                req_id: "r6".into(),
                caller_pane: "pane-a".into(),
                question: "Deploy to prod?".into(),
                choices: vec!["yes".into(), "no".into()],
                suggested: Some(0),
                multi: false,
                free_text: true,
            },
            Frame::AgentAskOk {
                req_id: "r6".into(),
                ask_id: "a1".into(),
            },
            Frame::AgentAskRequest {
                ask_id: "a1".into(),
                session: "sess".into(),
                pane: "pane-a".into(),
                question: "Deploy to prod?".into(),
                choices: vec!["yes".into(), "no".into()],
                suggested: Some(0),
                multi: false,
                free_text: true,
            },
            Frame::AgentAskAnswer {
                ask_id: "a1".into(),
                choices: vec![0],
                text: String::new(),
            },
            Frame::AgentAskStatus {
                req_id: "r7".into(),
                caller_pane: "pane-a".into(),
                ask_id: "a1".into(),
            },
            Frame::AgentAskStatusOk {
                req_id: "r7".into(),
                ask_id: "a1".into(),
                answered: true,
                choices: vec![0],
                text: String::new(),
                state: "answered".into(),
            },
        ];
        for f in frames {
            let lines = encode_frame(&f, "c1");
            let mut payload = String::new();
            for l in &lines {
                payload.push_str(l);
                payload.push('\n');
            }
            let mut d = Decoder::new();
            let got = d.feed(payload.as_bytes());
            assert_eq!(got, vec![f]);
        }
    }

    #[test]
    fn agent_registry_frames_roundtrip() {
        let frames = vec![
            Frame::AgentList {
                req_id: "r1".into(),
            },
            Frame::AgentListOk {
                req_id: "r1".into(),
                agents: vec![AgentInfo {
                    id: "aa".into(),
                    name: "iris".into(),
                    avatar_url: None,
                    home_machine: Some("omarchy".into()),
                    last_active: None,
                    conversation_count: Some(3),
                    primary_profile: Some("p-1".into()),
                    memory_scope: Some("org".into()),
                    org_id: Some("myorg".into()),
                    visibility: Some("private".into()),
                    tools_allowlist: Some(vec!["bash".into(), "read".into()]),
                    extra_instructions: Some("be terse".into()),
                    paused: Some(false),
                }],
            },
            Frame::AgentTalk {
                req_id: "r2".into(),
                agent: "aa".into(),
                mode: "continue".into(),
                text: Some("hi".into()),
                title: None,
                fork_from: None,
            },
            Frame::AgentTalk {
                req_id: "r3".into(),
                agent: "aa".into(),
                mode: "fork".into(),
                text: None,
                title: Some("branch".into()),
                fork_from: Some("cc".into()),
            },
            Frame::AgentTalkOk {
                req_id: "r2".into(),
                session: "ss".into(),
                pane: "pp".into(),
            },
            Frame::AgentBusy {
                req_id: "r4".into(),
                agent: "aa".into(),
            },
            Frame::AgentBusyOk {
                req_id: "r4".into(),
                busy: true,
                current_conversation: Some("cc".into()),
            },
            Frame::AgentBusyOk {
                req_id: "r5".into(),
                busy: false,
                current_conversation: None,
            },
            Frame::AgentActivity {
                req_id: "r6".into(),
                agent: None,
                kinds: None,
            },
            Frame::AgentActivityOk {
                req_id: "r6".into(),
                items: vec![ActivityItem {
                    id: "i1".into(),
                    kind: "task".into(),
                    agent: Some("aa".into()),
                    text: "done".into(),
                    ts: "2026-10-07T00:00:00Z".into(),
                }],
            },
            Frame::ActivityPush {
                items: vec![ActivityItem {
                    id: "ask/77".into(),
                    kind: "ask".into(),
                    agent: None,
                    text: "approve spawn?".into(),
                    ts: "2026-10-07T00:01:00Z".into(),
                }],
            },
            Frame::ActivityPush { items: vec![] },
            Frame::AgentPause {
                req_id: "r7".into(),
                agent: Some("iris".into()),
                paused: true,
            },
            Frame::AgentPause {
                req_id: "r8".into(),
                agent: None,
                paused: false,
            },
            Frame::AgentPauseOk {
                req_id: "r7".into(),
                results: vec![AgentPauseResult {
                    agent: "a1".into(),
                    name: "iris".into(),
                    forge: Some(true),
                    mule: Some(3),
                    error: None,
                }],
            },
            Frame::AgentProvision {
                req_id: "r9".into(),
                name: "Atlas".into(),
                avatar: Some("https://x/a.png".into()),
                profile_id: Some("p1".into()),
                memory_scope: "org".into(),
                org_id: Some("myorg".into()),
                home_machine: "machine-42".into(),
                tools_allowlist: vec!["bash".into()],
                wake_cron: Some("0 7 * * *".into()),
                wake_prompt: Some("Daily check-in for Atlas.".into()),
            },
            Frame::AgentProvision {
                req_id: "r10".into(),
                name: "Min".into(),
                avatar: None,
                profile_id: None,
                memory_scope: "agent".into(),
                org_id: None,
                home_machine: "m1".into(),
                tools_allowlist: vec![],
                wake_cron: None,
                wake_prompt: None,
            },
            Frame::AgentProvisionOk {
                req_id: "r9".into(),
                agent_id: "agent-1".into(),
                agent_name: "Atlas".into(),
                wake_id: Some("w-1".into()),
                wake_error: None,
            },
            Frame::AgentDelete {
                req_id: "r10".into(),
                agent_id: "agent-1".into(),
            },
            Frame::AgentDeleteOk {
                req_id: "r10".into(),
            },
            Frame::AgentUpdate {
                req_id: "r11".into(),
                agent_id: "agent-1".into(),
                name: Some("Atlas II".into()),
                avatar: None,
                profile_id: Some("p-9".into()),
                memory_scope: Some("org".into()),
                org_id: Some("myorg".into()),
                home_machine: None,
                visibility: None,
                tools_allowlist: Some(vec![]),
                extra_instructions: Some("be terse".into()),
            },
            Frame::AgentUpdate {
                req_id: "r12".into(),
                agent_id: "agent-2".into(),
                name: None,
                avatar: None,
                profile_id: None,
                memory_scope: None,
                org_id: None,
                home_machine: None,
                visibility: None,
                tools_allowlist: None,
                extra_instructions: None,
            },
            Frame::AgentUpdateOk {
                req_id: "r11".into(),
                agent: AgentInfo {
                    id: "agent-1".into(),
                    name: "Atlas II".into(),
                    avatar_url: None,
                    home_machine: None,
                    last_active: None,
                    conversation_count: None,
                    primary_profile: Some("p-9".into()),
                    memory_scope: Some("org".into()),
                    org_id: Some("myorg".into()),
                    visibility: Some("private".into()),
                    tools_allowlist: Some(vec![]),
                    extra_instructions: Some("be terse".into()),
                    paused: Some(true),
                },
            },
            Frame::WakeList {
                req_id: "r13".into(),
                agent_id: "agent-1".into(),
            },
            Frame::WakeListOk {
                req_id: "r13".into(),
                wakes: vec![WakeInfo {
                    id: "w-1".into(),
                    name: "wake-Atlas".into(),
                    kind: "cron".into(),
                    cron: Some("0 7 * * *".into()),
                    prompt: "Daily check-in for Atlas.".into(),
                    enabled: true,
                    last_fired_at: Some("2026-10-17T07:00:00Z".into()),
                    next_due_at: Some("2026-10-18T07:00:00Z".into()),
                }],
            },
            Frame::WakePut {
                req_id: "r14".into(),
                wake_id: Some("w-1".into()),
                agent_id: "agent-1".into(),
                cron: "30 6 * * 1-5".into(),
                prompt: "Standup.".into(),
                name: None,
                enabled: Some(false),
            },
            Frame::WakePutOk {
                req_id: "r14".into(),
                wake_id: "w-1".into(),
            },
            Frame::WakeDelete {
                req_id: "r15".into(),
                wake_id: "w-1".into(),
            },
            Frame::WakeDeleteOk {
                req_id: "r15".into(),
            },
            Frame::SandboxPackages {
                req_id: "r16".into(),
                agent_id: "agent-1".into(),
            },
            Frame::SandboxPackagesOk {
                req_id: "r16".into(),
                data: serde_json::json!({
                    "base_ready": true,
                    "approver": {"id": "ea-1", "name": "executive assistant"},
                    "pending": [{"id": "p1", "package": "nodejs", "reason": "tests"}],
                }),
            },
            Frame::SandboxDecide {
                req_id: "r17".into(),
                request_id: "p1".into(),
                approve: true,
                reason: None,
            },
            Frame::SandboxDecideOk {
                req_id: "r17".into(),
                data: serde_json::json!({"ok": true, "status": "approved", "package": "nodejs"}),
            },
            Frame::SandboxReset {
                req_id: "r18".into(),
                agent_id: "agent-1".into(),
            },
            Frame::SandboxResetOk {
                req_id: "r18".into(),
                rootfs_removed: 2,
            },
            Frame::SandboxRequest {
                req_id: "r19".into(),
                agent_id: "agent-1".into(),
                packages: vec!["nodejs".into(), "jq".into()],
                reason: Some("run the test suite".into()),
            },
            Frame::SandboxRequestOk {
                req_id: "r19".into(),
                data: serde_json::json!({"requested": [{"id": "p1", "package": "nodejs"}]}),
            },
        ];
        for f in frames {
            let lines = encode_frame(&f, "c1");
            let mut payload = String::new();
            for l in &lines {
                payload.push_str(l);
                payload.push('\n');
            }
            let mut d = Decoder::new();
            let got = d.feed(payload.as_bytes());
            assert_eq!(got, vec![f]);
        }
    }

    #[test]
    fn agent_registry_minimal_json() {
        // missing optional fields deserialize to None; wire field names
        // are stable (docs/PROTOCOL.md §8.6)
        let f: Frame =
            serde_json::from_str(r#"{"t":"AgentTalk","req_id":"r","agent":"a","mode":"new"}"#)
                .unwrap();
        match f {
            Frame::AgentTalk {
                text,
                title,
                fork_from,
                ..
            } => {
                assert!(text.is_none());
                assert!(title.is_none());
                assert!(fork_from.is_none());
            }
            _ => panic!("wrong variant"),
        }
        let f: Frame = serde_json::from_str(
            r#"{"t":"AgentListOk","req_id":"r","agents":[{"id":"a","name":"iris"}]}"#,
        )
        .unwrap();
        match f {
            Frame::AgentListOk { agents, .. } => {
                let a = &agents[0];
                assert_eq!(a.name, "iris");
                assert!(a.avatar_url.is_none());
                assert!(a.home_machine.is_none());
                assert!(a.last_active.is_none());
                assert!(a.conversation_count.is_none());
            }
            _ => panic!("wrong variant"),
        }
        let s: String = serde_json::to_string(&Frame::AgentActivityOk {
            req_id: "r".into(),
            items: vec![],
        })
        .unwrap();
        assert!(!s.contains("items"), "empty items list must be omitted");
    }

    #[test]
    fn activity_push_and_agent_pause_minimal_json() {
        // minimal AgentPause: agent defaults to None (pause the herd)
        let f: Frame =
            serde_json::from_str(r#"{"t":"AgentPause","req_id":"r","paused":true}"#).unwrap();
        match f {
            Frame::AgentPause { agent, paused, .. } => {
                assert!(agent.is_none());
                assert!(paused);
            }
            _ => panic!("wrong variant"),
        }
        // AgentPauseOk with missing optionals in a result
        let f: Frame = serde_json::from_str(
            r#"{"t":"AgentPauseOk","req_id":"r","results":[{"agent":"a","name":"iris"}]}"#,
        )
        .unwrap();
        match f {
            Frame::AgentPauseOk { results, .. } => {
                let r = &results[0];
                assert!(r.forge.is_none());
                assert!(r.mule.is_none());
                assert!(r.error.is_none());
            }
            _ => panic!("wrong variant"),
        }
        // ActivityPush with no items decodes to an empty set
        let f: Frame = serde_json::from_str(r#"{"t":"ActivityPush"}"#).unwrap();
        match f {
            Frame::ActivityPush { items } => assert!(items.is_empty()),
            _ => panic!("wrong variant"),
        }
        // additive: an old-client Meta (kind "activity" with no
        // payload — Meta's shape never changes) still parses; an
        // unknown frame tag is dropped, not fatal, by the decoder
        let f: Frame =
            serde_json::from_str(r#"{"t":"Meta","session":"s","kind":"activity","status":"x"}"#)
                .unwrap();
        assert!(matches!(f, Frame::Meta { .. }));
        let mut d = Decoder::new();
        let got = d.feed(b"{\"t\":\"Bogus\",\"x\":1}\n");
        assert!(got.is_empty());
    }

    #[test]
    fn agent_spawn_defaults() {
        // minimal JSON: mode/delivery/limit default; missing optional
        // fields deserialize to None/false
        let json = r#"{"t":"AgentSpawn","req_id":"r1","caller_pane":"p","caller_session":"s","kind":"pi","prompt":"hi"}"#;
        let f: Frame = serde_json::from_str(json).unwrap();
        match f {
            Frame::AgentSpawn {
                mode,
                callback,
                name,
                cwd,
                profile_id,
                ..
            } => {
                assert_eq!(mode, "split");
                assert!(!callback);
                assert!(name.is_none());
                assert!(cwd.is_none());
                assert!(profile_id.is_none());
            }
            _ => panic!("wrong variant"),
        }
        let json = r#"{"t":"AgentSend","req_id":"r","session":"s","pane":"p","text":"x"}"#;
        match serde_json::from_str::<Frame>(json).unwrap() {
            Frame::AgentSend { delivery, .. } => assert_eq!(delivery, "steer"),
            _ => panic!("wrong variant"),
        }
    }
}
