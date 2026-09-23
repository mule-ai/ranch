//! `ranchd` — the Ranch local daemon.
//!
//! Owns every terminal session on this machine: PTYs, ghostty-vt
//! emulators, and the state that keeps them alive after every client
//! disconnects. Clients (the `ranch` CLI today, the mobile app in M3)
//! speak the ranch-protocol frame set over a unix socket; the same frames
//! later flow through the Supabase relay unchanged (SPEC §4, PROTOCOL.md).
//!
//! M1 scope decisions (SPEC §12), as updated by M2.7:
//! - splits render as real side-by-side/stacked panes (weighted 50/50
//!   split tree, client draws rects from the Layout, focus ring on the
//!   active pane; Prefix+arrows move focus, Prefix+Ctrl-arrows resize)
//! - scrollback is a heuristic ring (lines observed scrolling off the
//!   top of the formatted screen)
//! - sessions do not survive a daemon restart (state.json is written for
//!   observability; true durability is post-M1)
//! - single-threaded blocking poll loop: unix socket, clients, pty masters
//! - when ~/.config/ranch/daemon.toml exists, a relay thread bridges the
//!   same frames to the machine's Supabase Realtime channel (relay.rs)


use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use libc::{SIGINT, WNOHANG, c_int, pollfd};
use ranch_protocol::{ChatMsg, Cursor, Decoder, Frame, Layout, PaneSnap, PiSessionInfo, SessionMeta};
use ranch_vt::Vt;
use uuid::Uuid;

#[path = "agenttools.rs"]
pub(crate) mod agenttools;
#[path = "control_api.rs"]
pub mod control_api;
#[path = "mule.rs"]
mod mule;
#[path = "triggers.rs"]
mod triggers;

const TICK_MS: i32 = 30;
/// Max file size the editor will read/write (M10). Keeps relay frames
/// chunked but bounded.
const FILE_MAX_BYTES: u64 = 256 * 1024;
const SCROLLBACK_CAP: usize = 2000;
const POLLIN: i16 = 0x001;
const POLLHUP: i16 = 0x00200;
const POLLERR: i16 = 0x00004;

/// Log with the daemon prefix (also used by the relay module).
pub fn clog(msg: &str) {
    eprintln!("ranchd: {msg}");
}

/// Convert unix seconds to (year, month, day, hour, min, sec) UTC.
pub fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, h, mi, s)
}

// ---------- pty FFI ----------

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

unsafe extern "C" {
    fn openpty(
        amaster: *mut c_int,
        aslave: *mut c_int,
        name: *mut u8,
        termp: *const libc::termios,
        winp: *const Winsize,
    ) -> c_int;
    fn fork() -> c_int;
    fn execvp(name: *const u8, argv: *const *const u8) -> c_int;
    fn setsid() -> c_int;
    fn tcsetpgrp(fd: c_int, pgid: c_int) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn ioctl(fd: c_int, req: libc::c_ulong, ...) -> c_int;
}

const TIOCSWINSZ: libc::c_ulong = 0x5414;

// ---------- model ----------

struct Pane {
    kind: String,
    master: RawFd,
    child: c_int,
    vt: Vt,
    prev_screen: Vec<String>,
    scrollback: VecDeque<String>,
    dirty: bool,
    seq: u64,
    dead: bool,
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.child > 0 {
            unsafe { kill(self.child, 15) };
        }
        unsafe {
            libc::close(self.master);
        }
    }
}

/// A forge-chat pane (M8): no PTY — the conversation lives in forge's
/// message table; the forge worker thread polls and this cache holds
/// what we've seen (for snapshots).
struct ChatPane {
    forge_sid: Uuid,
    cols: u16,
    rows: u16,
    chat: Vec<ChatMsg>,
    /// active-model display name, when known (filled by `meta
    /// kind="model"` broadcasts; surfaced in `PaneSnap.model`)
    model: Option<String>,
    /// latest context-window readout (filled by `meta kind="context"`
    /// broadcasts; surfaced in `PaneSnap.context`)
    context: Option<String>,
    /// working dir (local-pi panes: where the rpc child was spawned;
    /// recorded so restore can respawn in the same place)
    cwd: Option<String>,
}

struct Window {
    id: Uuid,
    name: String,
    /// Binary split tree. The root leaf is the first pane; PaneSplit
    /// replaces the split target's leaf with a new node. `pct` is the
    /// percent of space given to `a`.
    layout: Layout,
}

impl Window {
    fn new(layout: Layout, name: String) -> Self {
        Window {
            id: Uuid::new_v4(),
            name,
            layout,
        }
    }
    /// Replace the leaf `target` with a split whose second child is
    /// `new_id`. Returns true when found.
    fn split_leaf(&mut self, target: &str, new_id: &str, dir: u8) -> bool {
        fn walk(l: &mut Layout, target: &str, new_id: &str, dir: u8) -> bool {
            match l {
                Layout::Leaf { pane } if pane == target => {
                    let nb = Layout::Leaf {
                        pane: new_id.to_string(),
                    };
                    let na = std::mem::replace(
                        l,
                        Layout::Leaf {
                            pane: String::new(),
                        },
                    );
                    *l = Layout::Split {
                        dir,
                        a: Box::new(na),
                        b: Box::new(nb),
                        pct: 50,
                    };
                    true
                }
                Layout::Split { a, b, .. } => {
                    walk(a, target, new_id, dir) || walk(b, target, new_id, dir)
                }
                _ => false,
            }
        }
        walk(&mut self.layout, target, new_id, dir)
    }

    /// Remove a dead pane's leaf, promoting its sibling. Returns the
    /// promoted sibling pane id.
    fn remove_leaf(&mut self, pane: &str) -> Option<Uuid> {
        fn walk(l: &mut Layout, pane: &str) -> Option<Uuid> {
            match l {
                Layout::Split { a, b, .. } => {
                    let a_leaf = matches!(&**a, Layout::Leaf { pane: p } if p == pane);
                    let b_leaf = matches!(&**b, Layout::Leaf { pane: p } if p == pane);
                    if a_leaf || b_leaf {
                        let keep = if a_leaf { b.as_mut() } else { a.as_mut() };
                        let promoted = std::mem::replace(
                            keep,
                            Layout::Leaf {
                                pane: String::new(),
                            },
                        );
                        *l = promoted;
                        // caller resolves the sibling pane id below
                        None
                    } else {
                        walk(a, pane).or_else(|| walk(b, pane))
                    }
                }
                _ => None,
            }
        }
        walk(&mut self.layout, pane);
        // after collapse, find any leaf (the promoted subtree's first leaf)
        fn first_leaf(l: &Layout) -> Option<Uuid> {
            match l {
                Layout::Leaf { pane } => Uuid::parse_str(pane).ok(),
                Layout::Split { a, b, .. } => first_leaf(a).or_else(|| first_leaf(b)),
            }
        }
        first_leaf(&self.layout)
    }

    /// Swap the pane ids at the leaves holding `a` and `b`: the two
    /// rectangles trade places while each pane keeps its own PTY/VT
    /// state. Returns true when both leaves were found. Done as three
    /// rename passes through a unique sentinel so the walk can never
    /// confuse the two leaves mid-swap.
    fn swap_leaves(&mut self, a: &str, b: &str) -> bool {
        if a == b || a.is_empty() || b.is_empty() {
            return false;
        }
        fn rename(l: &mut Layout, from: &str, to: &str) -> bool {
            match l {
                Layout::Leaf { pane } if pane == from => {
                    *pane = to.to_string();
                    true
                }
                Layout::Split { a, b, .. } => rename(a, from, to) || rename(b, from, to),
                _ => false,
            }
        }
        let sentinel = Uuid::new_v4().to_string();
        if !rename(&mut self.layout, a, &sentinel) {
            return false;
        }
        if !rename(&mut self.layout, b, a) {
            rename(&mut self.layout, &sentinel, a); // restore on failure
            return false;
        }
        rename(&mut self.layout, &sentinel, b)
    }

    /// Panes present in this window's layout tree.
    fn pane_ids(&self) -> Vec<Uuid> {
        let mut out = Vec::new();
        fn walk(l: &Layout, out: &mut Vec<Uuid>) {
            match l {
                Layout::Leaf { pane } => {
                    if let Ok(pid) = Uuid::parse_str(pane) {
                        out.push(pid);
                    }
                }
                Layout::Split { a, b, .. } => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        walk(&self.layout, &mut out);
        out
    }
}

struct Session {
    id: Uuid,
    name: String,
    kind: String,
    panes: BTreeMap<Uuid, Pane>,
    /// Active pane (within the active window).
    active: Uuid,
    size: (u16, u16),
    /// Window stack; exactly one window (index `win`) is active.
    windows: Vec<Window>,
    win: usize,
    /// Forge-chat panes keyed by pane uuid (M8). Chat leaves live in
    /// the layout like PTY leaves; PTY-specific loops skip them
    /// (nothing here intersects `panes`).
    chats: BTreeMap<Uuid, ChatPane>,
}

impl Session {
    /// Remove `pane` from whichever window holds it; drop windows that
    /// run out of panes (active window index follows). Returns true when
    /// no windows remain (caller kills the session).
    fn remove_pane_everywhere(&mut self, pane: &str) -> bool {
        for i in 0..self.windows.len() {
            if self.windows[i]
                .pane_ids()
                .iter()
                .any(|p| p.to_string() == pane)
            {
                self.windows[i].remove_leaf(pane);
                break;
            }
        }
        let mut i = 0;
        while i < self.windows.len() {
            if self.windows[i].pane_ids().is_empty() {
                self.windows.remove(i);
                if self.win >= i && self.win > 0 {
                    self.win -= 1;
                }
            } else {
                i += 1;
            }
        }
        if self.windows.is_empty() {
            return true;
        }
        if self.win >= self.windows.len() {
            self.win = self.windows.len() - 1;
        }
        let ids = self.win().pane_ids();
        if !ids.contains(&self.active) {
            self.active = ids.first().copied().unwrap_or_else(Uuid::nil);
        }
        false
    }

    fn win(&self) -> &Window {
        &self.windows[self.win]
    }
    fn win_mut(&mut self) -> &mut Window {
        let i = self.win.min(self.windows.len().saturating_sub(1));
        &mut self.windows[i]
    }

    /// Compute each pane's (cols, rows) by walking the split tree.
    /// dir 1 = vertical split (left/right, pct to a), dir 0 = horizontal
    /// (top/bottom, pct to a).
    fn pane_sizes(&self) -> Vec<(Uuid, u16, u16)> {
        let mut out = Vec::new();
        fn walk(l: &Layout, x: u16, y: u16, w: u16, h: u16, out: &mut Vec<(Uuid, u16, u16)>) {
            match l {
                Layout::Leaf { pane } => {
                    if let Ok(pid) = Uuid::parse_str(pane) {
                        out.push((pid, w.max(1), h.max(1)));
                    }
                }
                Layout::Split { dir, a, b, pct } => {
                    let pct = (*pct as u16).clamp(1, 99);
                    if *dir == 1 {
                        let lw = (w * pct / 100).max(1).min(w.saturating_sub(1).max(1));
                        walk(a, x, y, lw, h, out);
                        walk(b, x + lw, y, w - lw, h, out);
                    } else {
                        let th = (h * pct / 100).max(1).min(h.saturating_sub(1).max(1));
                        walk(a, x, y, w, th, out);
                        walk(b, x, y + th, w, h - th, out);
                    }
                }
            }
        }
        walk(&self.win().layout, 0, 0, self.size.0, self.size.1, &mut out);
        out
    }

    /// Apply computed sizes to every pane's PTY + VT (with SIGWINCH).
    fn apply_sizes(&mut self) {
        // bootstrap: root leaf may still point at the nil placeholder
        if let Layout::Leaf { pane } = &self.win().layout {
            if Uuid::parse_str(&pane).map(|u| u.is_nil()).unwrap_or(true) {
                if let Some(first) = self.panes.keys().next().copied() {
                    self.win_mut().layout = Layout::Leaf {
                        pane: first.to_string(),
                    };
                }
            }
        }
        let sizes = self.pane_sizes();
        for (pid, cols, rows) in sizes {
            if let Some(p) = self.panes.get_mut(&pid) {
                if p.vt.dims() != (cols, rows) {
                    p.vt.resize(cols, rows);
                    let win = Winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ..Default::default()
                    };
                    unsafe { ioctl(p.master, TIOCSWINSZ, &win) };
                    p.prev_screen.clear();
                    p.dirty = true;
                }
            }
        }
    }

    /// Resize the split containing `pane` along `dir` by `delta` cells,
    /// clamped so every leaf keeps >= 4 cells. Two passes: locate the
    /// node + geometry immutably, then update its pct.
    fn resize_split(&mut self, pane: Uuid, dir: u8, delta: i16) {
        let pref = pane.to_string();
        fn find(l: &Layout, pane: &str) -> Option<(u8, String)> {
            match l {
                Layout::Leaf { .. } => None,
                Layout::Split { dir, a, b, .. } => {
                    let a_leaf = match &**a {
                        Layout::Leaf { pane: p } => Some(p.clone()),
                        _ => None,
                    };
                    let b_leaf = match &**b {
                        Layout::Leaf { pane: p } => Some(p.clone()),
                        _ => None,
                    };
                    if a_leaf.as_deref() == Some(pane) || b_leaf.as_deref() == Some(pane) {
                        fn fl2(l: &Layout) -> Option<String> {
                            match l {
                                Layout::Leaf { pane } => Some(pane.clone()),
                                Layout::Split { a, b, .. } => fl2(a).or_else(|| fl2(b)),
                            }
                        }
                        fl2(a).map(|f| (*dir, f))
                    } else {
                        find(a, pane).or_else(|| find(b, pane))
                    }
                }
            }
        }
        let Some((ndir, a_leaf)) = find(&self.win().layout, &pref) else {
            return;
        };
        if ndir != dir {
            return;
        }
        fn is_a(l: &Layout, pane: &str) -> Option<bool> {
            match l {
                Layout::Leaf { .. } => None,
                Layout::Split { a, b, .. } => {
                    let a_leaf = match &**a {
                        Layout::Leaf { pane: p } => Some(p.clone()),
                        _ => None,
                    };
                    if a_leaf.as_deref() == Some(pane) {
                        return Some(true);
                    }
                    if matches!(&**b, Layout::Leaf { pane: p } if p == pane) {
                        return Some(false);
                    }
                    is_a(a, pane).or_else(|| is_a(b, pane))
                }
            }
        }
        let a_is_target = is_a(&self.win().layout, &pref).unwrap_or(false);
        let sizes = self.pane_sizes();
        let axis = if dir == 1 { self.size.0 } else { self.size.1 };
        let a_size = Uuid::parse_str(&a_leaf)
            .ok()
            .and_then(|pid| sizes.iter().find(|(id, _, _)| *id == pid))
            .map(|(_, c, r)| if dir == 1 { *c } else { *r })
            .unwrap_or(axis / 2);
        let new_a = (a_size as i32
            + if a_is_target {
                delta as i32
            } else {
                -(delta as i32)
            })
        .clamp(4, axis as i32 - 4) as u16;
        let new_pct = ((new_a as u32 * 100) / axis.max(1) as u32).clamp(1, 99) as u8;
        fn set_pct(l: &mut Layout, pane: &str, dir: u8, new_pct: u8) -> bool {
            match l {
                Layout::Leaf { .. } => false,
                Layout::Split { dir: d, a, b, pct } => {
                    let a_hits = matches!(&**a, Layout::Leaf { pane: p } if p == pane);
                    let b_hits = matches!(&**b, Layout::Leaf { pane: p } if p == pane);
                    if (a_hits || b_hits) && *d == dir {
                        *pct = new_pct;
                        true
                    } else {
                        set_pct(a, pane, dir, new_pct) || set_pct(b, pane, dir, new_pct)
                    }
                }
            }
        }
        set_pct(&mut self.win_mut().layout, &pref, dir, new_pct);
    }
}

struct Client {
    /// Local clients read frames here; the relay client has None (its
    /// frames arrive via the relay pipe, read separately in the loop).
    stream: Option<UnixStream>,
    /// Frames written here are broadcast to remote clients over the relay.
    /// Set only for the relay client.
    relay_out: Option<std::fs::File>,
    /// Remote frames land on this pipe (read end); set only for the relay
    /// client. The write end lives in the relay thread.
    relay_in: Option<std::fs::File>,
    /// Control-API pseudo-client: frames "sent" here are collected and
    /// returned to the agent's HTTP request. Set only for the transient
    /// ctl client (drained per control request).
    sink: Option<std::sync::Arc<std::sync::Mutex<Vec<Frame>>>>,
    decoder: Decoder,
    name: String,
    /// Session currently attached; None = not attached.
    attach: Option<Uuid>,
    /// When set (via `Attach.chat_limit`), snapshots for this client
    /// truncate each chat pane's history to the last N rows; the client
    /// pages back with `ChatHistory`. Set by mobile to keep remote
    /// snapshots small.
    chat_limit: Option<usize>,
    scrollback_mode: bool,
    /// Editor file watches (M10 ph3): path -> last seen mtime, seeded by
    /// FileRead/FileWriteOk. The tick loop stats these and pushes
    /// FileChanged when the file moves on disk under the client.
    file_watches: BTreeMap<String, i64>,
}

struct Daemon {
    machine: String,
    socket_path: PathBuf,
    listener: UnixListener,
    sessions: BTreeMap<Uuid, Session>,
    clients: BTreeMap<RawFd, Client>,
    state_path: PathBuf,
    /// send session mirror ops to the relay thread
    mirror_tx: Option<std::sync::mpsc::Sender<relay::RelayOut>>,
    /// child pids whose Pane was removed before exit — reaped by the poll loop
    orphans: Vec<c_int>,
    /// forge worker jobs (None when forge is not configured)
    forge_tx: Option<std::sync::mpsc::Sender<forge::ForgeJob>>,
    /// local pi agents (pane -> child handle) for kind="pi" chat panes
    pi_agents: BTreeMap<Uuid, std::sync::Arc<pilocal::LocalPi>>,
    /// clone of the forge pipe writer — local-pi panes emit their
    /// Chat/Meta frames through the same pipe (the pre-match resolves
    /// + broadcasts them identically)
    forge_pipe_w: Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>,
    /// agent tool surface (Phase A): spawned-pane ownership + callbacks
    spawns: agenttools::SpawnRegistry,
    /// agent ask-user questions (Phase A2): pending ask_id -> record
    asks: agenttools::AskRegistry,
    /// `[agents] spawn_policy` from daemon.toml (default allow)
    spawn_policy: agenttools::SpawnPolicy,
    /// mule worker jobs (None when mule is not configured)
    mule_tx: Option<std::sync::mpsc::Sender<mule::MuleJob>>,
    /// trigger scheduler (Phase D): owns the trigger registry; fires
    /// workflows via mule_tx
    triggers: triggers::Scheduler,
    /// Whether to monitor external pi sessions (started outside ranch)
    /// and include them in PiList responses.
    monitor_external_pi: bool,
}

// ---------- helpers ----------

/// A pane child's current working directory (Linux: /proc symlink).
fn pane_cwd(child: c_int) -> Option<PathBuf> {
    let target = std::fs::read_link(format!("/proc/{child}/cwd")).ok()?;
    Some(target)
}

/// Clear FD_CLOEXEC on fd so it survives an execve (hot-upgrade path).
/// Returns the fd unchanged (for chaining).
fn fd_inherit(fd: RawFd) -> RawFd {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    fd
}

/// Read a pi session JSONL and return (id, title, cwd).
///
/// Title is the session's LAST user message (48-char truncate) so long,
/// continued sessions keep a current, identifying title instead of a stale
/// first prompt. Bounded reads: 64 KB head (session header) + 512 KB tail
/// (recent messages) so huge session files stay cheap.
fn pi_session_meta(path: &str) -> (Option<String>, Option<String>, Option<String>) {
    use std::io::{BufRead as _, Seek as _, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None, None),
    };
    let size = file.metadata().ok().map(|m| m.len()).unwrap_or(0);

    let mut sid: Option<String> = None;
    let mut cwd: Option<String> = None;

    // Pass 1: session header (id + cwd) from the head.
    {
        let mut reader = std::io::BufReader::new(&file);
        let mut bytes_read = 0u64;
        let mut line = String::new();
        while bytes_read < 64 * 1024 {
            line.clear();
            let n = match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(n) => n as u64,
            };
            bytes_read += n;
            if sid.is_some() && cwd.is_some() {
                break;
            }
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                if json.get("type").and_then(|t| t.as_str()) == Some("session") {
                    if sid.is_none() {
                        sid = json.get("id").and_then(|x| x.as_str()).map(String::from);
                    }
                    if cwd.is_none() {
                        cwd = json.get("cwd").and_then(|c| c.as_str()).map(String::from);
                    }
                }
            }
        }
    }

    // Pass 2: last user message from the tail (last 512 KB).
    let mut title: Option<String> = None;
    if file.seek(SeekFrom::Start(size.saturating_sub(512 * 1024))).is_ok() {
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // A partial first line (seek mid-line) fails JSON parse; skipped.
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                if json.get("type").and_then(|t| t.as_str()) == Some("message")
                    && json
                        .get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(|r| r.as_str())
                        == Some("user")
                {
                    let text = json
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| {
                            if let Some(arr) = c.as_array() {
                                arr.iter().find_map(|b| b.get("text").and_then(|t| t.as_str()))
                            } else {
                                c.as_str()
                            }
                        })
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !text.is_empty() {
                        let truncated: String = text.chars().take(48).collect();
                        title = Some(if text.chars().count() > 48 {
                            format!("{truncated}…")
                        } else {
                            truncated
                        });
                    }
                }
            }
        }
    }

    (sid, title, cwd)
}

/// mtime of a file as a unix-seconds string (empty when unavailable).
fn file_mtime_secs(path: &str) -> String {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

fn default_socket_path() -> PathBuf {
    home_dir().join(".local/state/ranch/daemon.sock")
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

/// Returns true if a process is currently accepting connections on this
/// unix socket path. A live listener completes the connect at the kernel
/// level; a stale socket file left behind by a crashed/killed process
/// yields ECONNREFUSED. Any other error is treated as occupied (safe
/// default: never unlink a socket we can't prove is dead).
fn socket_has_live_listener(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(stream) => {
            // A live daemon accepted the probe. Dropping the stream makes
            // the peer see a client that connected and immediately
            // disconnected — harmless.
            drop(stream);
            true
        }
        Err(e) => e.kind() != std::io::ErrorKind::ConnectionRefused
            && e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// `allow_local_pi = "false"` in ~/.config/ranch/daemon.toml disables
/// kind=pi session creation (local pi agent panes). Public demo
/// machines set this: the public must not reach any agent code path
/// but the restricted-key forge chat pane. Default true (local pi is
/// a normal feature on personal machines).
fn local_pi_allowed() -> bool {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return true,
    };
    let text = match std::fs::read_to_string(
        std::path::PathBuf::from(home).join(".config/ranch/daemon.toml"),
    ) {
        Ok(t) => t,
        Err(_) => return true,
    };
    for line in text.lines() {
        let line = line.trim();
        let rest = match line.strip_prefix("allow_local_pi") {
            Some(r) => r.trim_start(),
            None => continue,
        };
        if rest.is_empty() || !rest.starts_with('=') {
            continue; // e.g. `allow_local_pi_x`
        }
        let v = rest[1..].trim().trim_matches('"');
        return !v.eq_ignore_ascii_case("false");
    }
    true
}

// SAFETY: takes ownership of a raw fd from pipe2; caller guarantees the
// fd is not otherwise owned.
fn fd_file(fd: RawFd) -> std::fs::File {
    use std::os::fd::FromRawFd;
    unsafe { std::fs::File::from_raw_fd(fd) }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "ranch".into())
}

/// Build version reported in HelloOk: CI sets RANCH_VERSION (git-sha
/// based, e.g. "main-82c0643" — the same string as the release index);
/// local/source builds report "dev".
pub fn build_version() -> String {
    std::env::var("RANCH_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| option_env!("RANCH_VERSION").map(String::from))
        .unwrap_or_else(|| "dev".into())
}

/// Read each attached file (capped at 50 KiB per file) and build an
/// augmented prompt: the file contents are prepended so the agent can
/// see them immediately without a tool call.
fn build_attached_prompt(text: &str, attachments: &[String]) -> String {
    if attachments.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    for path in attachments {
        let p = std::path::Path::new(path);
        match std::fs::read(p) {
            Ok(bytes) => {
                let cap = 50 * 1024; // 50 KiB
                let content = if bytes.len() > cap {
                    let mut truncated = String::from_utf8_lossy(&bytes[..cap]).into_owned();
                    truncated.push_str("\n… [truncated at 50 KiB]");
                    truncated
                } else {
                    String::from_utf8_lossy(&bytes).into_owned()
                };
                let name = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
                out.push_str(&format!("[Attached file: {} ({} bytes)]\n```
{}
```
\n", name, bytes.len(), content));
            }
            Err(e) => {
                out.push_str(&format!("[Attached file: {} — read error: {}]
\n", path, e));
            }
        }
    }
    out.push_str(text);
    out
}

fn home_dir_string() -> String {
    home_dir().to_string_lossy().into_owned()
}

/// The Tier-1 persistence document (state.json contents / manifest state
/// section): sessions with panes, chats, windows, layouts.
fn state_value(
    sessions: &BTreeMap<Uuid, Session>,
    pi_agents: &BTreeMap<Uuid, std::sync::Arc<pilocal::LocalPi>>,
    spawns: Option<&agenttools::SpawnRegistry>,
    triggers: Option<&triggers::Scheduler>,
    monitor_external_pi: bool,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "machine": hostname(),
        "monitor_external_pi": monitor_external_pi,
        "sessions": sessions.iter().map(|(id, s)| serde_json::json!({
            "id": id.to_string(),
            "name": s.name,
            "kind": s.kind,
            "active": s.active.to_string(),
            "size": [s.size.0, s.size.1],
            "win": s.win,
            "windows": s.windows.iter().map(|w| serde_json::json!({
                "id": w.id.to_string(),
                "name": w.name,
                "layout": w.layout,
            })).collect::<Vec<_>>(),
            "panes": s.panes.iter().map(|(pid, p)| serde_json::json!({
                "id": pid.to_string(),
                "kind": p.kind,
                "dead": p.dead,
                "cwd": pane_cwd(p.child),
            })).collect::<Vec<_>>(),
            "chats": s.chats.iter().map(|(pid, cp)| serde_json::json!({
                "id": pid.to_string(),
                "forge_sid": if cp.forge_sid.is_nil() { None } else { Some(cp.forge_sid.to_string()) },
                "cwd": cp.cwd,
                // local-pi backing: pi's own session file, so the
                // respawned rpc child can switch back into the same
                // conversation
                "pi_session_file": cp.forge_sid.is_nil().then(|| {
                    pi_agents.get(pid).and_then(|a| {
                        a.session_file.lock().ok().and_then(|g| g.clone())
                    })
                }).flatten(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    if let Some(reg) = spawns {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("spawns".into(), agenttools::registry_value(reg));
        }
    }
    if let Some(trig) = triggers {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("triggers".into(), trig.to_value());
        }
    }
    v
}

/// Drop layout leaves that reference panes which no longer exist
/// (restore path: a pane that failed to respawn must not leave a dangling
/// leaf, or snapshots + sizes walk over ghosts).
fn prune_layout(
    l: &mut Layout,
    panes: &BTreeMap<Uuid, Pane>,
    chats: &BTreeMap<Uuid, ChatPane>,
) -> bool {
    match l {
        Layout::Leaf { pane } => {
            let alive = Uuid::parse_str(pane)
                .map(|u| panes.contains_key(&u) || chats.contains_key(&u))
                .unwrap_or(false);
            if !alive {
                *pane = Uuid::nil().to_string();
            }
            alive
        }
        Layout::Split { a, b, .. } => {
            let a_alive = prune_layout(a, panes, chats);
            let b_alive = prune_layout(b, panes, chats);
            if a_alive && b_alive {
                true
            } else if a_alive {
                *l = std::mem::replace(
                    a.as_mut(),
                    Layout::Leaf {
                        pane: Uuid::nil().to_string(),
                    },
                );
                true
            } else if b_alive {
                *l = std::mem::replace(
                    b.as_mut(),
                    Layout::Leaf {
                        pane: Uuid::nil().to_string(),
                    },
                );
                true
            } else {
                false
            }
        }
    }
}

/// Spawn a new shell pane. The CALLER must insert the layout split
/// (via `Session::split_leaf`) before or after; the PTY is sized with
/// `session.size` and `apply_sizes` fixes it after the split is made.
fn spawn_pane(session: &mut Session, pane_kind: &str, cwd: Option<&str>) -> Result<Uuid, String> {
    let (cols, rows) = session.size;
    let vt = Vt::new(cols, rows).map_err(|e| format!("vt: {e}"))?;
    let mut amaster: c_int = 0;
    let mut aslave: c_int = 0;
    let win = Winsize {
        ws_row: rows,
        ws_col: cols,
        ..Default::default()
    };
    if unsafe {
        openpty(
            &mut amaster,
            &mut aslave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &win,
        )
    } != 0
    {
        return Err("openpty failed".into());
    }
    let pid = unsafe { fork() };
    if pid == 0 {
        // child
        unsafe {
            setsid();
            tcsetpgrp(aslave, 0);
            libc::dup2(aslave, 0);
            libc::dup2(aslave, 1);
            libc::dup2(aslave, 2);
            if aslave > 2 {
                libc::close(aslave);
            }
            libc::close(amaster);
            libc::setenv(
                b"TERM\0".as_ptr() as *const libc::c_char,
                b"xterm-256color\0".as_ptr() as *const libc::c_char,
                1,
            );
            // agent sessions run `pi` via a login shell (so the mise
            // shim PATH from the user profile applies); shells run bare.
            // CString: execvp takes a C string, and `String::as_ptr()` is
            // not NUL-terminated (UB: first spawn may read a lucky byte,
            // later ones read past the allocation).
            let shell = default_shell();
            let shell_c = match std::ffi::CString::new(shell.clone()) {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("ranchd: SHELL contains NUL; cannot spawn pane");
                    std::process::exit(1);
                }
            };
            let shell_p: *const u8 = shell_c.as_ptr() as *const u8;
            if pane_kind == "forge" {
                let dir = cwd.map(std::path::PathBuf::from).unwrap_or_else(home_dir);
                let d_c =
                    std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap_or_default();
                if libc::chdir(d_c.as_ptr()) != 0 {
                    eprintln!("ranchd: chdir {:?} failed", dir);
                }
                let arg = b"exec pi\0";
                let argv: [*const u8; 4] =
                    [shell_p, b"-lc\0".as_ptr(), arg.as_ptr(), std::ptr::null()];
                if execvp(shell_p, argv.as_ptr()) != 0 {
                    eprintln!("ranchd: execvp {shell:?} failed: {:?}", std::io::Error::last_os_error());
                }
            } else {
                let argv: [*const u8; 2] = [shell_p, std::ptr::null()];
                if execvp(shell_p, argv.as_ptr()) != 0 {
                    eprintln!("ranchd: execvp {shell:?} failed: {:?}", std::io::Error::last_os_error());
                }
            }
            std::process::exit(1);
        }
    }
    unsafe {
        libc::close(aslave);
    }
    vt.attach_pty(amaster); // terminal query responses flow back to the child
    let id = Uuid::new_v4();
    session.panes.insert(
        id,
        Pane {
            kind: pane_kind.to_string(),
            master: amaster,
            child: pid,
            vt,
            prev_screen: vec![],
            scrollback: VecDeque::with_capacity(SCROLLBACK_CAP),
            dirty: true,
            seq: 0,
            dead: false,
        },
    );
    session.active = id;
    Ok(id)
}

/// Build a full snapshot frame for a session (client field filled per-recipient).
fn snapshot_session(s: &Session, chat_limit: Option<usize>) -> Option<Frame> {
    let sizes: std::collections::HashMap<Uuid, (u16, u16)> = s
        .pane_sizes()
        .into_iter()
        .map(|(pid, c, r)| (pid, (c, r)))
        .collect();
    let mut snaps = Vec::new();
    let mut active_seq: u64 = 0;
    for (pid, p) in &s.panes {
        let (cols, rows) = sizes.get(pid).copied().unwrap_or(s.size);
        let lines = p.vt.screen();
        let (cx, cy, vis) = p.vt.cursor();
        let snap = PaneSnap {
            id: pid.to_string(),
            cols,
            rows,
            lines,
            seq: p.seq,
            cursor: Some(Cursor {
                x: cx,
                y: cy,
                visible: vis,
            }),
            kind: Some("pty".into()),
            chat: None,
            forge_session: None,
            model: None,
            context: None,
            chat_has_more: None,
            cwd: pane_cwd(p.child).map(|p| p.to_string_lossy().into_owned()),
        };
        if *pid == s.active {
            active_seq = p.seq;
        }
        snaps.push(snap);
    }
    for (pid, cp) in &s.chats {
        let (cols, rows) = sizes.get(pid).copied().unwrap_or((cp.cols, cp.rows));
        // Remote clients (mobile/web) attach with a chat_limit so the
        // snapshot carries only the last N rows — long conversations
        // would otherwise ship as megabytes over Realtime. Older rows
        // are paged back with `ChatHistory`.
        let chat_has_more = chat_limit
            .map(|n| cp.chat.len() > n)
            .unwrap_or(false);
        let chat = match chat_limit {
            Some(n) if n > 0 => {
                cp.chat.iter().rev().take(n).collect::<Vec<_>>().into_iter().rev().cloned().collect()
            }
            _ => cp.chat.clone(),
        };
        snaps.push(PaneSnap {
            id: pid.to_string(),
            cols,
            rows,
            lines: vec![],
            seq: 0,
            cursor: None,
            kind: Some("forge-chat".into()),
            chat: Some(chat),
            forge_session: Some(cp.forge_sid.to_string()),
            model: cp.model.clone(),
            context: cp.context.clone(),
            chat_has_more: if chat_has_more { Some(true) } else { None },
            cwd: None,
        });
    }
    Some(Frame::Snapshot {
        id: Uuid::new_v4().to_string(),
        client: String::new(), // filled per-recipient
        session: s.id.to_string(),
        seq: active_seq,
        layout: s.win().layout.clone(),
        active_pane: s.active.to_string(),
        panes: snaps, // each pane carries its own seq
        meta: vec![],
        windows: s
            .windows
            .iter()
            .map(|w| ranch_protocol::WindowSnap {
                id: w.id.to_string(),
                name: w.name.clone(),
                layout: w.layout.clone(),
            })
            .collect(),
        window: s.windows.get(s.win).map(|w| w.id.to_string()),
    })
}

/// Serialize + chunk a frame and write all bytes to the client's sink.
fn send_frame(c: &mut Client, frame: &Frame) {
    let cid = Uuid::new_v4().to_string();
    for line in ranch_protocol::encode_frame(frame, &cid) {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let res = match (&mut c.stream, &mut c.relay_out) {
            (Some(s), _) => s.write_all(&bytes),
            (None, Some(w)) => w.write_all(&bytes),
            // control pseudo-client: collect (the request thread returns
            // them in the HTTP response)
            (None, None) if c.sink.is_some() => {
                if let Some(sink) = &c.sink {
                    if let Ok(mut g) = sink.lock() {
                        g.push(frame.clone());
                    }
                }
                Ok(())
            }
            (None, None) => Ok(()),
        };
        if let Err(e) = res {
            eprintln!("ranchd: write: {e}");
            break;
        }
    }
}

// ---------- agent tools (Phase A) ----------

// Caller-pane resolution. Human clients (unix socket / relay) are nil
// = unrestricted. Agents calling through the loopback control API
// identify their pane via the `X-Ranch-Pane` header; the control API
// forwards it in `ControlRequest.caller_pane`, and frames built from
// control requests carry it in the frame's caller_pane field. The
// thread-local below supports callers that construct frames on agent
// threads; regular daemon frames default to nil.
thread_local! {
    static CALLER_PANE: std::cell::RefCell<Option<Uuid>> = const { std::cell::RefCell::new(None) };
}

#[allow(dead_code)] // reserved: forge-bridge request threads set this
pub fn set_caller_pane(pane: Option<Uuid>) {
    CALLER_PANE.with(|c| *c.borrow_mut() = pane);
}

pub fn caller_pane() -> Uuid {
    CALLER_PANE.with(|c| c.borrow().unwrap_or(Uuid::nil()))
}

impl Daemon {
    /// The effective caller for a frame from client fd `from`. The
    /// control API runs each request on a thread with CALLER_PANE set;
    /// normal client frames are human (nil).
    /// forge-required-but-unconfigured error reply.
    fn forge_unconfigured(&mut self, from: RawFd, req_id: &str) {
        if let Some(c) = self.clients.get_mut(&from) {
            send_frame(
                c,
                &Frame::Error {
                    req_id: Some(req_id.to_string()),
                    message: "forge not configured (set forge_api_key in daemon.toml)".into(),
                },
            );
        }
    }

    /// relay-required-but-unconfigured error reply (webhook CRUD needs
    /// the machine's Supabase credentials).
    fn relay_unconfigured(&mut self, from: RawFd, req_id: &str) {
        if let Some(c) = self.clients.get_mut(&from) {
            send_frame(
                c,
                &Frame::Error {
                    req_id: Some(req_id.to_string()),
                    message: "relay not configured (run `ranch register`)".into(),
                },
            );
        }
    }

    /// mule-required-but-unconfigured error reply.
    fn mule_unconfigured(&mut self, from: RawFd, req_id: &str) {
        if let Some(c) = self.clients.get_mut(&from) {
            send_frame(
                c,
                &Frame::Error {
                    req_id: Some(req_id.to_string()),
                    message: "mule not configured (set mule_url in daemon.toml)".into(),
                },
            );
        }
    }

    /// The effective caller for an agent-tool frame: the frame's own
    /// caller_pane field (control API / forge bridge stamp it), falling
    /// back to the thread-local (reserved) and finally nil = human.
    fn frame_caller(frame_caller: &str, _req_id: &str) -> Uuid {
        if let Ok(u) = Uuid::parse_str(frame_caller) {
            if !u.is_nil() {
                return u;
            }
        }
        caller_pane()
    }

    /// Resolve which session/pane a chat pane id lives in.
    fn find_chat(&self, pid: Uuid) -> Option<Uuid> {
        self.sessions
            .iter()
            .find_map(|(sid, s)| s.chats.contains_key(&pid).then_some(*sid))
    }

    /// Send a frame to every client attached to `sid` (plus the
    /// originating client fd even if detached).
    fn broadcast_to_session(&mut self, sid: Uuid, frame: &Frame, also: Option<RawFd>) {
        let mut recipients: Vec<RawFd> = self
            .clients
            .iter()
            .filter(|(_, c)| c.attach == Some(sid))
            .map(|(f, _)| *f)
            .collect();
        if let Some(fd) = also {
            if !recipients.contains(&fd) {
                recipients.push(fd);
            }
        }
        for rfd in recipients {
            if let Some(c) = self.clients.get_mut(&rfd) {
                send_frame(c, frame);
            }
        }
    }

    /// Deliver an AgentDone: broadcast to the caller's session + inject
    /// the system row into the caller's chat pane cache so re-attaches
    /// still see it. Also clears the spawn record when terminal.
    fn deliver_agent_done(
        &mut self,
        caller_pane: Uuid,
        spawn: &agenttools::SpawnRecord,
        spawned_session: Uuid,
        spawned_pane: Uuid,
        outcome: &str,
        last_row: Option<ChatMsg>,
    ) {
        let row = agenttools::agent_done_row(spawn, spawned_pane, outcome, last_row.as_ref());
        // inject into the caller pane's chat cache (if it's a chat pane)
        let caller_sid = self.find_chat(caller_pane);
        if let Some(cs) = caller_sid {
            if let Some(s) = self.sessions.get_mut(&cs) {
                if let Some(cp) = s.chats.get_mut(&caller_pane) {
                    cp.chat.push(row.clone());
                }
            }
        }
        let frame = agenttools::agent_done_frame(spawn, spawned_session, spawned_pane, outcome, last_row);
        match caller_sid {
            Some(cs) => self.broadcast_to_session(cs, &frame, None),
            None => {
                // caller pane gone: best-effort broadcast everywhere
                let fds: Vec<RawFd> = self.clients.keys().copied().collect();
                for fd in fds {
                    if let Some(c) = self.clients.get_mut(&fd) {
                        send_frame(c, &frame);
                    }
                }
            }
        }
        // "closed"/"denied"/"timeout" release the spawn record;
        // "completed" keeps it (the caller can still steer/close the
        // pane after the first turn)
        if matches!(outcome, "closed" | "denied" | "timeout") {
            self.spawns.remove_pane(spawned_pane);
            self.write_state();
        }
    }

    /// Frame::AgentSpawn — create the pane, register it, send the
    /// initial prompt, ack. Honors the policy gate.
    #[allow(clippy::too_many_arguments)]
    fn handle_agent_spawn(
        &mut self,
        from: RawFd,
        req_id: &str,
        caller_pane: &str,
        caller_session: &str,
        kind: &str,
        name: &Option<String>,
        cwd: &Option<String>,
        prompt: &str,
        mode: &str,
        callback: bool,
    ) {
        let caller = Uuid::parse_str(caller_pane).unwrap_or(Uuid::nil());
        let reply_err = |d: &mut Self, msg: String| {
            if let Some(c) = d.clients.get_mut(&from) {
                send_frame(
                    c,
                    &Frame::Error {
                        req_id: Some(req_id.to_string()),
                        message: msg,
                    },
                );
            }
        };
        if kind != "pi" && kind != "forge" {
            reply_err(self, format!("unknown agent kind {kind:?} (pi|forge)"));
            return;
        }
        if kind == "pi" && !local_pi_allowed() {
            reply_err(self, "local pi agents are disabled on this machine".into());
            return;
        }
        match self.spawn_policy {
            agenttools::SpawnPolicy::Deny => {
                reply_err(self, "agent spawns are disabled on this machine".into());
                return;
            }
            agenttools::SpawnPolicy::Ask => {
                if !caller.is_nil() {
                    // agent-initiated: hold for human approval
                    let spawn_id = Uuid::new_v4().to_string();
                    let rec = agenttools::SpawnRecord {
                        spawn_id: spawn_id.clone(),
                        caller_pane: caller,
                        created_at: Instant::now(),
                        callback,
                        callback_fired: false,
                    };
                    let preview: String = prompt.chars().take(120).collect();
                    self.spawns.pending.insert(spawn_id.clone(), (rec, Instant::now()));
                    let req = Frame::AgentSpawnRequest {
                        spawn_id,
                        caller_pane: caller.to_string(),
                        kind: kind.to_string(),
                        preview,
                    };
                    let fds: Vec<RawFd> = self.clients.keys().copied().collect();
                    for fd in fds {
                        if let Some(c) = self.clients.get_mut(&fd) {
                            send_frame(c, &req);
                        }
                    }
                    return; // resolution arrives via AgentSpawnApprove
                }
                // humans bypass `ask` (they just clicked the thing)
            }
            agenttools::SpawnPolicy::Allow => {}
        }

        let spawn_id = Uuid::new_v4().to_string();
        let anchor: Option<String> = if caller.is_nil() {
            cwd.clone()
        } else {
            // agent spawns default to the caller's cwd, overridable
            cwd.clone().or_else(|| {
                let caller = Uuid::parse_str(caller_pane).unwrap_or(Uuid::nil());
                self.sessions.values().find_map(|s| {
                    s.panes.get(&caller).and_then(|p| pane_cwd(p.child)).map(|p| p.to_string_lossy().to_string())
                })
            })
        };

        let result: Result<(Uuid, Uuid), String> = if mode == "session" {
            self.spawn_agent_session(kind, name.clone(), anchor, prompt, callback, spawn_id.clone())
        } else {
            self.spawn_agent_split(kind, caller_session, anchor, prompt, callback, spawn_id.clone())
        };
        match result {
            Ok((sid, pid)) => {
                // register + send the initial prompt
                let rec = agenttools::SpawnRecord {
                    spawn_id: spawn_id.clone(),
                    caller_pane: caller,
                    created_at: Instant::now(),
                    callback,
                    callback_fired: false,
                };
                if let Err(e) = self.spawns.register(pid, rec) {
                    reply_err(self, e);
                    return;
                }
                if !prompt.is_empty() {
                    self.send_agent_prompt(pid, prompt);
                }
                self.write_state();
                eprintln!(
                    "ranchd: agent spawn {spawn_id}: pane {pid} ({kind}, mode={mode}, caller={caller_pane})"
                );
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::AgentSpawnOk {
                            req_id: req_id.to_string(),
                            spawn_id,
                            session: sid.to_string(),
                            pane: pid.to_string(),
                        },
                    );
                }
            }
            Err(e) => {
                eprintln!("ranchd: agent spawn failed: {e}");
                reply_err(self, e);
            }
        }
    }

    /// mode="split": a new chat pane in the caller's session (or the
    /// session named by caller_session).
    #[allow(clippy::too_many_arguments)]
    fn spawn_agent_split(
        &mut self,
        kind: &str,
        session_ref: &str,
        anchor: Option<String>,
        _prompt: &str,
        _callback: bool,
        _spawn_id: String,
    ) -> Result<(Uuid, Uuid), String> {
        let sid = self
            .resolve_session(session_ref)
            .map(|s| s.id)
            .or_else(|| self.find_chat(Uuid::parse_str(session_ref).unwrap_or(Uuid::nil())))
            .ok_or_else(|| format!("caller session {session_ref:?} not found"))?;
        let s = self.sessions.get_mut(&sid).ok_or("session vanished")?;
        let dir = anchor.as_deref().map(std::path::PathBuf::from);
        // pick the split target: the caller pane when it lives here,
        // else the active pane
        let caller = caller_pane();
        let target = if !caller.is_nil() && s.chats.contains_key(&caller) {
            caller
        } else {
            s.active
        };
        let pid = Uuid::new_v4();
        match kind {
            "pi" => {
                let dir_str = dir
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
                let pipe_w = self
                    .forge_pipe_w
                    .clone()
                    .ok_or("no agent pipe (forge disabled?)")?;
                pilocal::LocalPi::spawn(pid, &dir_str, pilocal::no_tools_configured(), pipe_w, &mut self.pi_agents)?;
                s.chats.insert(
                    pid,
                    ChatPane {
                        forge_sid: Uuid::nil(),
                        cols: 80,
                        rows: 24,
                        chat: vec![],
                        model: None,
                        context: None,
                        cwd: Some(dir_str),
                    }
                );
            }
            "forge" => {
                let fcfg = forge::load_forge_config().ok_or("forge not configured")?;
                let forge_sid = forge::create_forge_session(
                    &fcfg,
                    "agent-spawned",
                    dir.as_deref().map(|p| p.to_string_lossy()).as_deref(),
                    None,
                )?;
                s.chats.insert(
                    pid,
                    ChatPane {
                        forge_sid,
                        cols: 80,
                        rows: 24,
                        chat: vec![],
                        model: None,
                        context: None,
                        cwd: dir.map(|p| p.to_string_lossy().to_string()),
                    }
                );
                if let Some(tx) = &self.forge_tx {
                    let _ = tx.send(forge::ForgeJob::Watch { pane: pid, forge_sid });
                }
            }
            _ => return Err(format!("unknown kind {kind:?}")),
        }
        s.win_mut().split_leaf(&target.to_string(), &pid.to_string(), 1);
        s.active = pid;
        s.apply_sizes();
        self.resnap(&sid);
        Ok((sid, pid))
    }

    /// mode="session": a full agent session (kind pi/forge), mirroring
    /// the SessionsCreate agent path.
    #[allow(clippy::too_many_arguments)]
    fn spawn_agent_session(
        &mut self,
        kind: &str,
        name: Option<String>,
        cwd: Option<String>,
        _prompt: &str,
        _callback: bool,
        _spawn_id: String,
    ) -> Result<(Uuid, Uuid), String> {
        let id = Uuid::new_v4();
        let name = name.unwrap_or_else(|| format!("agent-{}", &id.to_string()[..4]));
        let pid = Uuid::new_v4();
        let forge_sid = match kind {
            "pi" => Uuid::nil(),
            "forge" => {
                let fcfg = forge::load_forge_config().ok_or("forge not configured")?;
                forge::create_forge_session(
                    &fcfg,
                    &name,
                    cwd.as_deref(),
                    None,
                )?
            }
            _ => return Err(format!("unknown kind {kind:?}")),
        };
        let mut s = Session {
            id,
            name: name.clone(),
            kind: kind.to_string(),
            panes: BTreeMap::new(),
            active: Uuid::nil(),
            size: (80, 24),
            windows: vec![Window::new(
                Layout::Leaf { pane: pid.to_string() },
                "0".into(),
            )],
            win: 0,
            chats: BTreeMap::new(),
        };
        if kind == "pi" {
            let dir_str = cwd
                .clone()
                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
            let pipe_w = self
                .forge_pipe_w
                .clone()
                .ok_or("no agent pipe (forge disabled?)")?;
            pilocal::LocalPi::spawn(pid, &dir_str, pilocal::no_tools_configured(), pipe_w, &mut self.pi_agents)?;
            s.chats.insert(
                pid,
                ChatPane {
                    forge_sid: Uuid::nil(),
                    cols: 80,
                    rows: 24,
                    chat: vec![],
                    model: None,
                    context: None,
                    cwd: Some(dir_str),
                }
            );
        } else {
            s.chats.insert(
                pid,
                ChatPane {
                    forge_sid,
                    cols: 80,
                    rows: 24,
                    chat: vec![],
                    model: None,
                    context: None,
                    cwd,
                }
            );
            if let Some(tx) = &self.forge_tx {
                let _ = tx.send(forge::ForgeJob::Watch { pane: pid, forge_sid });
            }
        }
        s.active = pid;
        let kind_out = kind.to_string();
        self.sessions.insert(id, s);
        self.mirror(relay::RelayOut::UpsertSession {
            id: id.to_string(),
            name,
            kind: kind_out,
        });
        Ok((id, pid))
    }

    /// Send a prompt to a chat pane's agent (shared by spawn + AgentSend).
    fn send_agent_prompt(&mut self, pid: Uuid, text: &str) {
        let backing = self.sessions.values().find_map(|s| s.chats.get(&pid).map(|cp| cp.forge_sid));
        match backing {
            Some(fsid) if fsid.is_nil() => {
                if let Some(lp) = self.pi_agents.get(&pid) {
                    if let Some(pipe_w) = &self.forge_pipe_w {
                        if let Err(e) = lp.prompt(pipe_w, text, text, &[]) {
                            eprintln!("ranchd: pi prompt failed: {e}");
                        }
                    }
                }
            }
            Some(fsid) => {
                if let Some(tx) = &self.forge_tx {
                    let _ = tx.send(forge::ForgeJob::Send {
                        pane: pid,
                        forge_sid: fsid,
                        text: text.to_string(),
                    });
                }
            }
            None => eprintln!("ranchd: send_agent_prompt: pane {pid} not found"),
        }
    }

    fn handle_agent_send(
        &mut self,
        from: RawFd,
        req_id: &str,
        caller_pane: &str,
        session: &str,
        pane: &str,
        text: &str,
        delivery: &str,
    ) {
        let _ = delivery; // steer vs queue: forge has no queue concept; pi steer == prompt-while-working
        let Ok(pid) = Uuid::parse_str(pane) else { return };
        let caller = Self::frame_caller(caller_pane, req_id);
        if !self.spawns.authorized(caller, pid) {
            if let Some(c) = self.clients.get_mut(&from) {
                send_frame(
                    c,
                    &Frame::Error {
                        req_id: Some(req_id.to_string()),
                        message: "not your pane".into(),
                    },
                );
            }
            return;
        }
        let _ = session;
        self.send_agent_prompt(pid, text);
        if let Some(c) = self.clients.get_mut(&from) {
            // ack via status (send has no dedicated ok; the chat rows
            // coming back are the confirmation)
            send_frame(
                c,
                &Frame::AgentStatusOk {
                    req_id: req_id.to_string(),
                    pane: pid.to_string(),
                    state: "working".into(),
                    model: None,
                },
            );
        }
    }

    fn handle_agent_close(
        &mut self,
        from: RawFd,
        req_id: &str,
        caller_pane: &str,
        session: &str,
        pane: &str,
    ) {
        let Ok(pid) = Uuid::parse_str(pane) else { return };
        let caller = Self::frame_caller(caller_pane, req_id);
        if !self.spawns.authorized(caller, pid) {
            if let Some(c) = self.clients.get_mut(&from) {
                send_frame(
                    c,
                    &Frame::Error {
                        req_id: Some(req_id.to_string()),
                        message: "not your pane".into(),
                    },
                );
            }
            return;
        }
        let sid = match self.find_chat(pid) {
            Some(s) => s,
            None => {
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::Error {
                            req_id: Some(req_id.to_string()),
                            message: format!("pane {pane} not found"),
                        },
                    );
                }
                return;
            }
        };
        let _ = session;
        // last-row snapshot for the callback, then close
        let last = self
            .sessions
            .get(&sid)
            .and_then(|s| s.chats.get(&pid))
            .and_then(|cp| cp.chat.iter().rev().find(|m| m.role == "assistant"))
            .cloned();
        let rec = self.spawns.get_by_pane(pid).cloned();
        // kill the backing agent + remove the pane (tmux semantics via
        // remove_pane_everywhere: killing the last pane kills the
        // session — that's the right default for agent fan-out too)
        if let Some(s) = self.sessions.get_mut(&sid) {
            if s.chats.remove(&pid).is_some() {
                if let Some(lp) = self.pi_agents.remove(&pid) {
                    if let Some(pid) = lp.kill() {
                        self.orphans.push(pid);
                    }
                }
                if let Some(tx) = &self.forge_tx {
                    let _ = tx.send(forge::ForgeJob::Unwatch { pane: pid });
                }
                s.remove_pane_everywhere(&pid.to_string());
                s.apply_sizes();
            }
        }
        // last chat pane gone (or window collapse) -> kill the session
        let session_dead = self.sessions.get(&sid).is_none_or(|s| {
            s.windows.is_empty()
                || s.win().pane_ids().is_empty()
        });
        if session_dead {
            if let Some(s) = self.sessions.remove(&sid) {
                for p in s.panes.values() {
                    self.orphans.push(p.child);
                }
                for cpid in s.chats.keys() {
                    if let Some(lp) = self.pi_agents.remove(cpid) {
                        if let Some(pid) = lp.kill() {
                            self.orphans.push(pid);
                        }
                    }
                    if let Some(tx) = &self.forge_tx {
                        let _ = tx.send(forge::ForgeJob::Unwatch { pane: *cpid });
                    }
                }
                self.mirror(relay::RelayOut::DeleteSession {
                    id: sid.to_string(),
                });
                let gone = Frame::Meta {
                    session: sid.to_string(),
                    pane: None,
                    kind: "exited".into(),
                    status: Some("session ended".into()),
                };
                let recipients: Vec<RawFd> = self.clients.iter().map(|(f, _)| *f).collect();
                for rfd in recipients {
                    if let Some(c) = self.clients.get_mut(&rfd) {
                        if c.attach == Some(sid) {
                            c.attach = None;
                        }
                        send_frame(c, &gone);
                    }
                }
            }
        } else {
            self.resnap(&sid);
        }
        self.write_state();
        // ack + callback (AgentDone "closed")
        if let Some(c) = self.clients.get_mut(&from) {
            send_frame(
                c,
                &Frame::AgentStatusOk {
                    req_id: req_id.to_string(),
                    pane: pid.to_string(),
                    state: "closed".into(),
                    model: None,
                },
            );
        }
        if let Some(rec) = rec {
            // deliver the callback to the spawner (skip if closing
            // self-spawned-and-self is the same pane — can't happen)
            self.deliver_agent_done(rec.caller_pane, &rec, sid, pid, "closed", last);
        }
    }

    fn handle_agent_approve(&mut self, from: RawFd, spawn_id: String, allow: bool) {
        let _ = from; // any attached client may resolve
        let Some((rec, _)) = self.spawns.pending.remove(spawn_id.as_str()) else {
            return;
        };
        if !allow {
            self.deliver_agent_done(rec.caller_pane, &rec, Uuid::nil(), Uuid::nil(), "denied", None);
            return;
        }
        // approved: re-dispatch the original spawn via the stored record.
        // The prompt was never stored (approval carries only a preview),
        // so an approved spawn creates the pane WITHOUT a prompt; the
        // extension re-sends the prompt as AgentSend after the ack. This
        // keeps the approval payload small (no prompt in every client's
        // face).
        let _pid = Uuid::new_v4();
        let kind = "pi".to_string(); // approved spawns use the local runtime
        // NOTE: approval re-dispatch: create in the caller's session
        let caller_session = self
            .find_chat(rec.caller_pane)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let result = self.spawn_agent_split(
            kind.as_str(),
            &caller_session,
            None,
            "",
            rec.callback,
            spawn_id.clone(),
        );
        if let Ok((sid, pid)) = result {
            let mut rec2 = rec.clone();
            rec2.spawn_id = spawn_id.to_string();
            if let Err(e) = self.spawns.register(pid, rec2.clone()) {
                eprintln!("ranchd: approved spawn register failed: {e}");
                return;
            }
            self.write_state();
            // ack to the (now-forgotten) requester is impossible — the
            // requester was an agent loopback connection that's gone;
            // instead broadcast AgentSpawnOk so the extension can
            // correlate by spawn_id.
            let ok = Frame::AgentSpawnOk {
                req_id: spawn_id.to_string(),
                spawn_id: spawn_id.to_string(),
                session: sid.to_string(),
                pane: pid.to_string(),
            };
            let fds: Vec<RawFd> = self.clients.keys().copied().collect();
            for fd in fds {
                if let Some(c) = self.clients.get_mut(&fd) {
                    send_frame(c, &ok);
                }
            }
        }
    }

    // ---------- agent ask-user question (Phase A2) ----------

    /// Agent (via control API) or client asks the human a question.
    /// Registers a pending ask, broadcasts `AgentAskRequest` to every
    /// client (they render a prompt + may push a notification), and
    /// answers the requester with `AgentAskOk { ask_id }`. The agent's
    /// tool call stays blocked in the harness; it polls
    /// `AgentAskStatus` until a client answers (or the TTL expires).
    #[allow(clippy::too_many_arguments)]
    fn handle_agent_ask(
        &mut self,
        from: RawFd,
        req_id: &str,
        caller_pane: &str,
        question: &str,
        choices: Vec<String>,
        suggested: Option<usize>,
        multi: bool,
        free_text: bool,
    ) {
        let caller = Uuid::parse_str(caller_pane).unwrap_or(Uuid::nil());
        let session = self.find_chat(caller);
        let ask_id = Uuid::new_v4().to_string();
        let rec = agenttools::AskRecord {
            ask_id: ask_id.clone(),
            caller_pane: caller,
            session: session.unwrap_or(Uuid::nil()),
            answer: None,
            created_at: Instant::now(),
            answered_at: None,
        };
        self.asks.insert(rec.clone());

        // Broadcast the question to every attached client.
        let req = Frame::AgentAskRequest {
            ask_id: ask_id.clone(),
            session: rec.session.to_string(),
            pane: caller.to_string(),
            question: question.to_string(),
            choices,
            suggested,
            multi,
            free_text,
        };
        let fds: Vec<RawFd> = self.clients.keys().copied().collect();
        for fd in fds {
            if let Some(c) = self.clients.get_mut(&fd) {
                send_frame(c, &req);
            }
        }

        // Ack back to the requester (the control-API sink).
        if let Some(c) = self.clients.get_mut(&from) {
            send_frame(
                c,
                &Frame::AgentAskOk {
                    req_id: req_id.to_string(),
                    ask_id,
                },
            );
        }
    }

    /// A client answered a pending question. First answer wins; the
    /// answer is recorded and re-broadcast so every client can dismiss
    /// its prompt and show the chosen value.
    fn handle_agent_ask_answer(
        &mut self,
        ask_id: &str,
        choices: Vec<usize>,
        text: &str,
    ) {
        let Some(rec) = self.asks.get_mut(ask_id) else {
            return; // unknown or already pruned
        };
        if rec.answered_at.is_some() {
            return; // already answered (idempotent)
        }
        rec.answer = Some(agenttools::AskAnswer { choices: choices.clone(), text: text.to_string() });
        rec.answered_at = Some(Instant::now());

        // Re-broadcast the answer so all clients update/dismiss.
        let ans = Frame::AgentAskAnswer {
            ask_id: ask_id.to_string(),
            choices: choices.clone(),
            text: text.to_string(),
        };
        let fds: Vec<RawFd> = self.clients.keys().copied().collect();
        for fd in fds {
            if let Some(c) = self.clients.get_mut(&fd) {
                send_frame(c, &ans);
            }
        }
        eprintln!("ranchd: ask {ask_id} answered (choices={choices:?} text={text:?})");
    }

    /// Agent polls whether its question has been answered yet.
    fn handle_agent_ask_status(&mut self, from: RawFd, req_id: &str, caller_pane: &str, ask_id: &str) {
        let caller = Uuid::parse_str(caller_pane).unwrap_or(Uuid::nil());
        let (state, choices, text) = match self.asks.get(ask_id) {
            None => ("unknown".to_string(), Vec::new(), String::new()),
            Some(rec) => {
                // Ownership: only the asking pane (or a human, nil) may read.
                if !caller.is_nil() && rec.caller_pane != caller {
                    ("unknown".to_string(), Vec::new(), String::new())
                } else {
                    match &rec.answer {
                        Some(a) => ("answered".to_string(), a.choices.clone(), a.text.clone()),
                        None => {
                            let st = rec.state();
                            (st.to_string(), Vec::new(), String::new())
                        }
                    }
                }
            }
        };
        let answered = state == "answered";
        if let Some(c) = self.clients.get_mut(&from) {
            send_frame(
                c,
                &Frame::AgentAskStatusOk {
                    req_id: req_id.to_string(),
                    ask_id: ask_id.to_string(),
                    answered,
                    choices,
                    text,
                    state,
                },
            );
        }
    }
}

// ---------- daemon ----------

impl Daemon {
    /// Shared construction: socket bind + relay thread + forge worker.
    /// No state restoration — callers layer that on (new: Tier-1 restore;
    /// inherit: manifest adoption).
    fn base(socket_path: PathBuf, state_path: PathBuf) -> Result<Daemon, String> {
        let listener =
            UnixListener::bind(&socket_path).map_err(|e| format!("bind {socket_path:?}: {e}"))?;
        Daemon::base_with_listener(listener, state_path)
    }

    fn base_with_listener(listener: UnixListener, state_path: PathBuf) -> Result<Daemon, String> {
        let socket_path = default_socket_path();
        let spawn_policy = agenttools::SpawnPolicy::load();

        // relay: enabled only when `ranch register` has run (daemon.toml)
        let mut relay_client: Option<Client> = None;
        let mut mirror_tx = None;
        if let Some(cfg) = relay::load_config() {
            match relay::make_pipes() {
                Ok(((daemon_r, daemon_w), (relay_r, relay_w))) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    relay::spawn(cfg, daemon_w, relay_r, rx);
                    relay_client = Some(Client {
                        stream: None,
                        relay_out: Some(relay_w),
                        relay_in: Some(fd_file(daemon_r)),
                        sink: None,
                        decoder: Decoder::new(),
                        name: "relay".into(),
                        attach: None,
                        chat_limit: None,
                        scrollback_mode: false,
                        file_watches: BTreeMap::new(),
                    });
                    mirror_tx = Some(tx);
                }
                Err(e) => clog(&format!("relay: disabled: {e}")),
            }
        } else {
            clog("relay: disabled (no ~/.config/ranch/daemon.toml — run `ranch register`)");
        }

        eprintln!("ranchd: machine={}", hostname());
        let mut daemon = Daemon {
            machine: hostname(),
            socket_path,
            listener,
            sessions: BTreeMap::new(),
            clients: BTreeMap::new(),
            state_path,
            mirror_tx,
            orphans: Vec::new(),
            forge_tx: None,
            pi_agents: BTreeMap::new(),
            forge_pipe_w: None,
            spawns: agenttools::SpawnRegistry::new(),
            asks: agenttools::AskRegistry::new(),
            spawn_policy,
            mule_tx: None,
            triggers: triggers::Scheduler::new(None),
            monitor_external_pi: false,
        };
        // register the relay as a client keyed by its read-pipe fd; remote
        // frames arrive there and daemon->remote frames go out via relay_out
        if let Some(rc) = relay_client {
            let rfd = rc.relay_in.as_ref().map(|f| f.as_raw_fd());
            if let Some(rfd) = rfd {
                daemon.clients.insert(rfd, rc);
                clog(&format!("relay: enabled (client fd {rfd})"));
            }
        }

        // agent frame pipe: the forge worker AND local-pi reader threads
        // both write frames to this pipe; the poll loop reads them via the
        // "forge" client below. The pipe must exist even when the forge
        // API is not configured — local pi panes (kind "pi") use it too,
        // and without it they are dead on arrival on forge-less daemons.
        let forge_cfg = forge::load_forge_config();
        match relay::make_pipes() {
            Ok(((f_daemon_r, f_daemon_w), (_u1, _u2))) => {
                if let Ok(clone) = f_daemon_w.try_clone() {
                    daemon.forge_pipe_w =
                        Some(std::sync::Arc::new(std::sync::Mutex::new(clone)));
                }
                let fclient = Client {
                    stream: None,
                    relay_out: None,
                    relay_in: Some(fd_file(f_daemon_r)),
                    sink: None,
                    decoder: Decoder::new(),
                    name: "forge".into(),
                    attach: None,
                    chat_limit: None,
                    scrollback_mode: false,
                    file_watches: BTreeMap::new(),
                };
                if let Some(rfd) = fclient.relay_in.as_ref().map(|f| f.as_raw_fd()) {
                    daemon.clients.insert(rfd, fclient);
                    eprintln!("ranchd: agent pipe ready (fd {rfd})");
                }
                if let Some(fcfg) = forge_cfg {
                    let (tx, rx) = std::sync::mpsc::channel();
                    forge::spawn_worker(fcfg, f_daemon_w, rx);
                    eprintln!("ranchd: forge: worker started");
                    daemon.forge_tx = Some(tx);
                } else {
                    eprintln!("ranchd: forge: disabled (no forge_api_key in daemon.toml); agent pipe available for local pi panes");
                    // f_daemon_w drops here; the clone in forge_pipe_w
                    // keeps the pipe write side open for pi readers.
                }
            }
            Err(e) => {
                eprintln!("ranchd: agent pipe init failed: {e} (pi panes will not work)");
            }
        }

        // mule worker (Phase C): shares the agent pipe (its frames are
        // pane-addressed Chat/Meta rows + request replies; the pre-match
        // resolves + broadcasts them like forge's)
        if let Some(mcfg) = mule::load_mule_config() {
            if let Some(pipe_w) = &daemon.forge_pipe_w {
                if let Ok(clone) = pipe_w.lock().unwrap().try_clone() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    mule::spawn_worker(mcfg, clone, rx);
                    daemon.mule_tx = Some(tx);
                    eprintln!("ranchd: mule: worker started");
                }
            }
        } else {
            eprintln!("ranchd: mule: disabled (no mule_url in daemon.toml)");
        }
        // trigger scheduler rides the mule worker (Phase D)
        daemon.triggers = triggers::Scheduler::new(daemon.mule_tx.clone());
        Ok(daemon)
    }

    /// Cold start: full construction + Tier-1 restore from state.json.
    fn new() -> Result<Daemon, String> {
        let socket_path = default_socket_path();
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        if socket_path.exists() {
            if socket_has_live_listener(&socket_path) {
                return Err(format!(
                    "a ranch daemon is already listening on {} — refusing to start \
                     a second instance (it would steal the socket path and orphan \
                     the running daemon's listener). Stop the existing daemon \
                     first (systemctl --user stop ranchd), or hot-upgrade it \
                     (ranch upgrade).",
                    socket_path.display()
                ));
            }
            // No live listener: stale socket file from a crashed/killed
            // daemon — safe to remove.
            eprintln!("ranchd: removing stale socket {}", socket_path.display());
            std::fs::remove_file(&socket_path).ok();
        }
        let state_path = socket_path.parent().unwrap().join("state.json");
        let mut daemon = Daemon::base(socket_path, state_path)?;
        // Tier-1 persistence: rebuild sessions recorded in state.json
        // (chat panes re-attach to forge/local-pi, shells respawn fresh
        // in their last cwd). Runs after the forge worker exists so Watch
        // jobs and pi spawns can be issued.
        daemon.restore_state();
        Ok(daemon)
    }

    fn write_state(&self) {
        let v = state_value(&self.sessions, &self.pi_agents, Some(&self.spawns), Some(&self.triggers), self.monitor_external_pi);
        if let Ok(json) = serde_json::to_string_pretty(&v) {
            std::fs::write(&self.state_path, json).ok();
        }
    }

    /// Tier-1 session persistence: rebuild every recorded session on
    /// startup — windows/split layout verbatim (fresh window ids, same
    /// pane ids), chat panes re-attached to their backing (forge watch
    /// re-subscribes, local pi respawns + switch_session into the old
    /// conversation), shell panes fresh in their last cwd.
    /// Tier-2 hot upgrade: persist a manifest (state + fd map), clear
    /// CLOEXEC on every fd that must survive exec, then execve the SAME
    /// binary with `--inherit`. Child processes (shells, pi rpc) never
    /// notice — their slave side stays attached the whole time. Threads
    /// (relay, forge worker, pi readers) die with exec and are restarted
    /// by the inheriting instance.
    fn hot_upgrade(&mut self) -> Result<(), String> {
        let exe = std::fs::read_link("/proc/self/exe").map_err(|e| format!("self exe: {e}"))?;
        // `make install` replaces the binary file; the running daemon's
        // /proc/self/exe then points at a deleted inode and execvp gives
        // ENOENT. Strip the " (deleted)” suffix so we exec the NEW binary
        // from disk (which is exactly what a hot upgrade wants).
        let exe_str = exe.to_string_lossy();
        let exe: std::path::PathBuf = exe_str
            .strip_suffix(" (deleted)")
            .map(std::path::PathBuf::from)
            .unwrap_or(exe);
        let manifest_path = self.state_path.with_extension("manifest.json");

        let mut fds = serde_json::json!({ "panes": {}, "pi": [] });
        for (_sid, s) in &self.sessions {
            for (pid, p) in &s.panes {
                if p.dead {
                    continue;
                }
                fds["panes"][pid.to_string()] = serde_json::json!({
                    "fd": fd_inherit(p.master),
                    "child": p.child,
                });
            }
        }
        // pi rpc children: clear CLOEXEC on their stdin/stdout pipes so
        // the inheriting daemon can rebuild LocalPi around them
        for (pid, a) in &self.pi_agents {
            if let (Some(fin), Some(fout)) = (a.raw_stdin(), a.raw_stdout()) {
                fds["pi"].as_array_mut().unwrap().push(serde_json::json!({
                    "pane": pid.to_string(),
                    "stdin": fd_inherit(unsafe { libc::dup(fin) }),
                    "stdout": fd_inherit(unsafe { libc::dup(fout) }),
                    "pid": a.child_pid(),
                }));
            }
        }

        // manifest: Tier-1 state (same shape as state.json) + the fd map
        let state = state_value(&self.sessions, &self.pi_agents, Some(&self.spawns), Some(&self.triggers), self.monitor_external_pi);
        let manifest = serde_json::json!({ "state": state, "fds": fds });
        std::fs::write(
            &manifest_path,
            serde_json::to_string(&manifest).map_err(|e| format!("manifest encode: {e}"))?,
        )
        .map_err(|e| format!("manifest write: {e}"))?;

        eprintln!("ranchd: hot upgrade: exec-ing inherited daemon…");
        // pass the listening socket through too — the inheriting daemon
        // must NOT re-bind (the inherited listener still holds the path)
        let inherited_listener = fd_inherit(self.listener.as_raw_fd());
        // Leak the original listener so its fd isn't closed before exec
        // (exec never returns on success; Drop must not run). Swap in a
        // dummy owned listener for the struct — but keep the REAL listen
        // fd around: if execvp fails we must restore it, otherwise the
        // daemon polls a dummy fd (stdin) and spins at 100% CPU with a
        // dead listener (seen live).
        let real_listener_fd = self.listener.as_raw_fd();
        let real_listener_dup = fd_inherit(real_listener_fd); // CLOEXEC-cleared dup
        std::mem::forget(std::mem::replace(
            &mut self.listener,
            // SAFETY: dummy placeholder; on the success path exec replaces
            // the process image before this is ever used. FromRawFd for
            // UnixListener comes via std::os::unix::net.
            unsafe { <std::os::unix::net::UnixListener as std::os::fd::FromRawFd>::from_raw_fd(0) },
        ));
        let exe_c = std::ffi::CString::new(exe.as_os_str().as_encoded_bytes())
            .map_err(|_| "exe cstring".to_string())?;
        let arg1: *const libc::c_char = b"--inherit\0".as_ptr() as *const libc::c_char;
        let arg2 = std::ffi::CString::new(manifest_path.as_os_str().as_encoded_bytes())
            .map_err(|_| "manifest cstring".to_string())?;
        let listener_arg = std::ffi::CString::new(format!("--listen-fd={inherited_listener}"))
            .map_err(|_| "listen cstring".to_string())?;
        let argv = [
            exe_c.as_ptr(),
            arg1,
            arg2.as_ptr(),
            listener_arg.as_ptr(),
            std::ptr::null(),
        ];
        unsafe { libc::execvp(exe_c.as_ptr(), argv.as_ptr()) };
        // exec failed — restore the real listener from the CLOEXEC-cleared
        // dup and resume serving (self.listener now owns the dup)
        self.listener = unsafe {
            <std::os::unix::net::UnixListener as std::os::fd::FromRawFd>::from_raw_fd(real_listener_dup)
        };
        Err(format!(
            "execvp failed: {}",
            std::io::Error::last_os_error()
        ))
    }

    /// Hot-upgrade inherit path: adopt the previous generation's state
    /// (manifest = Tier-1 state + fd map). PTY masters and pi rpc pipes
    /// arrive as inherited fds — children were never interrupted.
    fn inherit(manifest_path: &str, listen_fd: Option<String>) -> Result<Daemon, String> {
        let text =
            std::fs::read_to_string(manifest_path).map_err(|e| format!("manifest read: {e}"))?;
        let manifest: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("manifest parse: {e}"))?;
        let state = manifest.get("state").ok_or("manifest: no state")?;
        let fds = manifest.get("fds").ok_or("manifest: no fds")?;
        let _ = std::fs::remove_file(manifest_path);

        // base daemon WITHOUT restore (we rebuild from the manifest)
        let socket_path = default_socket_path();
        let state_path = socket_path.parent().unwrap().join("state.json");
        let mut daemon = if let Some(fd) = &listen_fd {
            // adopt the previous generation's listening socket — it still
            // holds the path (bind would EADDRINUSE)
            let fd: RawFd = fd.parse().map_err(|_| "bad --listen-fd")?;
            use std::os::unix::io::FromRawFd;
            Daemon::base_with_listener(unsafe { UnixListener::from_raw_fd(fd) }, state_path)?
        } else {
            Daemon::base(socket_path, state_path)?
        };

        // pty pane fds
        let pane_fds: std::collections::HashMap<String, (RawFd, libc::pid_t)> = fds
            .get("panes")
            .and_then(|p| p.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(pid, v)| {
                        let fd = v.get("fd")?.as_i64()? as RawFd;
                        let child = v.get("child")?.as_i64()? as libc::pid_t;
                        Some((pid.clone(), (fd, child)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // pi agent fds
        let mut pi_fds: std::collections::HashMap<String, (RawFd, RawFd, Option<libc::pid_t>)> =
            std::collections::HashMap::new();
        if let Some(arr) = fds.get("pi").and_then(|p| p.as_array()) {
            for e in arr {
                let pane = e.get("pane").and_then(|x| x.as_str());
                let si = e.get("stdin").and_then(|x| x.as_i64());
                let so = e.get("stdout").and_then(|x| x.as_i64());
                if let (Some(pane), Some(si), Some(so)) = (pane, si, so) {
                    pi_fds.insert(
                        pane.to_string(),
                        (
                            si as RawFd,
                            so as RawFd,
                            e.get("pid")
                                .and_then(|x| x.as_i64())
                                .map(|x| x as libc::pid_t),
                        ),
                    );
                }
            }
        }

        let sessions = state
            .get("sessions")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        for sv in &sessions {
            let Some(sid) = sv
                .get("id")
                .and_then(|x| x.as_str())
                .and_then(|x| Uuid::parse_str(x).ok())
            else {
                continue;
            };
            let Some(name) = sv.get("name").and_then(|x| x.as_str()).map(String::from) else {
                continue;
            };
            let kind = sv
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("shell")
                .to_string();
            let size = sv
                .get("size")
                .and_then(|x| x.as_array())
                .and_then(|a| Some((a.first()?.as_u64()? as u16, a.get(1)?.as_u64()? as u16)))
                .unwrap_or((80, 24));
            let mut s = Session {
                id: sid,
                name: name.clone(),
                kind: kind.clone(),
                panes: BTreeMap::new(),
                active: Uuid::nil(),
                size,
                windows: vec![],
                win: 0,
                chats: BTreeMap::new(),
            };
            // adopt pty panes around their inherited master fds
            if let Some(panes) = sv.get("panes").and_then(|p| p.as_array()) {
                for pv in panes {
                    let Some(pid) = pv
                        .get("id")
                        .and_then(|x| x.as_str())
                        .and_then(|x| Uuid::parse_str(x).ok())
                    else {
                        continue;
                    };
                    if pv.get("dead").and_then(|x| x.as_bool()).unwrap_or(false) {
                        continue;
                    }
                    let Some((fd, child)) = pane_fds.get(&pid.to_string()).copied() else {
                        continue;
                    };
                    let (cols, rows) = size;
                    let vt = match Vt::new(cols, rows) {
                        Ok(vt) => vt,
                        Err(e) => {
                            eprintln!("ranchd: inherit: vt for pane {pid}: {e}");
                            continue;
                        }
                    };
                    vt.attach_pty(fd);
                    s.panes.insert(
                        pid,
                        Pane {
                            kind: pv
                                .get("kind")
                                .and_then(|x| x.as_str())
                                .unwrap_or("shell")
                                .to_string(),
                            master: fd,
                            child,
                            vt,
                            prev_screen: vec![],
                            scrollback: VecDeque::with_capacity(SCROLLBACK_CAP),
                            dirty: true,
                            seq: 0,
                            dead: false,
                        },
                    );
                }
            }
            // adopt chat panes: forge watches re-subscribe, local pi
            // children were inherited (rebuild LocalPi around their fds)
            if let Some(chats) = sv.get("chats").and_then(|c| c.as_array()) {
                for cv in chats {
                    let Some(pid) = cv
                        .get("id")
                        .and_then(|x| x.as_str())
                        .and_then(|x| Uuid::parse_str(x).ok())
                    else {
                        continue;
                    };
                    let fsid = cv
                        .get("forge_sid")
                        .and_then(|x| x.as_str())
                        .and_then(|x| Uuid::parse_str(x).ok())
                        .unwrap_or_else(Uuid::nil);
                    let cwd = cv.get("cwd").and_then(|x| x.as_str()).map(String::from);
                    s.chats.insert(
                        pid,
                        ChatPane {
                            forge_sid: fsid,
                            cols: 80,
                            rows: 24,
                            chat: vec![],
                            model: None,
                            context: None,
                            cwd: cwd.clone(),
                        }
                    );
                    if fsid.is_nil() {
                        if let Some((si, so, cpid)) = pi_fds.get(&pid.to_string()).copied() {
                            let pipe = daemon.forge_pipe_w.clone().ok_or("no forge pipe")?;
                            if let Err(e) = pilocal::LocalPi::from_inherited(
                                pid,
                                cwd.as_deref().unwrap_or("/"),
                                si,
                                so,
                                cpid,
                                pipe,
                                &mut daemon.pi_agents,
                            ) {
                                eprintln!("ranchd: inherit: pi pane {pid}: {e}");
                            } else {
                                eprintln!("ranchd: inherited local pi pane {pid} (pid {cpid:?})");
                            }
                        } else {
                            eprintln!(
                                "ranchd: inherit: no fds for pi pane {pid} — needs Tier-1 respawn"
                            );
                        }
                    } else if let Some(tx) = &daemon.forge_tx {
                        let _ = tx.send(forge::ForgeJob::Watch {
                            pane: pid,
                            forge_sid: fsid,
                        });
                        eprintln!("ranchd: inherited forge chat pane {pid} -> {fsid}");
                    }
                }
            }
            // windows/layout verbatim
            if let Some(windows) = sv.get("windows").and_then(|w| w.as_array()) {
                for wv in windows {
                    let wname = wv
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("0")
                        .to_string();
                    let layout = wv
                        .get("layout")
                        .cloned()
                        .and_then(|l| serde_json::from_value::<Layout>(l).ok())
                        .unwrap_or_else(|| Layout::Leaf {
                            pane: Uuid::nil().to_string(),
                        });
                    s.windows.push(Window {
                        id: Uuid::new_v4(),
                        name: wname,
                        layout,
                    });
                }
            }
            if s.windows.is_empty() {
                s.windows.push(Window::new(
                    Layout::Leaf {
                        pane: Uuid::nil().to_string(),
                    },
                    "0".into(),
                ));
            }
            s.win = (sv.get("win").and_then(|x| x.as_u64()).unwrap_or(0) as usize)
                .min(s.windows.len() - 1);
            let recorded_active = sv
                .get("active")
                .and_then(|x| x.as_str())
                .and_then(|x| Uuid::parse_str(x).ok());
            let win_ids = s.win().pane_ids();
            s.active = match recorded_active {
                Some(a) if win_ids.contains(&a) => a,
                _ => win_ids.first().copied().unwrap_or_else(Uuid::nil),
            };
            s.apply_sizes();
            eprintln!(
                "ranchd: inherited session {name} ({sid}) panes={} chats={}",
                s.panes.len(),
                s.chats.len()
            );
            daemon.sessions.insert(sid, s);
        }

        // mirror the inherited sessions so the phone list repopulates
        let ids: Vec<(String, String, String)> = daemon
            .sessions
            .values()
            .map(|s| (s.id.to_string(), s.name.clone(), s.kind.clone()))
            .collect();
        for (id, name, kind) in ids {
            daemon.mirror(relay::RelayOut::UpsertSession { id, name, kind });
        }
        Ok(daemon)
    }

    fn restore_state(&mut self) {
        let Ok(text) = std::fs::read_to_string(&self.state_path) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            eprintln!("ranchd: restore: state.json unparseable, starting empty");
            return;
        };
        // agent-tool spawn registry (ownership + callbacks survive restarts)
        self.spawns = agenttools::registry_from_value(&v);
        // trigger registry (Phase D)
        self.triggers = triggers::Scheduler::from_value(&v, self.mule_tx.clone());
        // pi monitor external sessions toggle
        self.monitor_external_pi = v.get("monitor_external_pi").and_then(|x| x.as_bool()).unwrap_or(false);
        let Some(sessions) = v.get("sessions").and_then(|s| s.as_array()) else {
            return;
        };
        let mut restored = 0usize;
        for sv in sessions {
            let (Some(sid), Some(name)) = (
                sv.get("id")
                    .and_then(|x| x.as_str())
                    .and_then(|x| Uuid::parse_str(x).ok()),
                sv.get("name").and_then(|x| x.as_str()).map(String::from),
            ) else {
                continue;
            };
            let kind = sv
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("shell")
                .to_string();
            let size = sv
                .get("size")
                .and_then(|x| x.as_array())
                .and_then(|a| Some((a.first()?.as_u64()? as u16, a.get(1)?.as_u64()? as u16)))
                .unwrap_or((80, 24));
            let win_idx = sv.get("win").and_then(|x| x.as_u64()).unwrap_or(0) as usize;

            let mut s = Session {
                id: sid,
                name: name.clone(),
                kind: kind.clone(),
                panes: BTreeMap::new(),
                active: Uuid::nil(),
                size,
                windows: vec![],
                win: 0,
                chats: BTreeMap::new(),
            };

            // --- chat panes (forge + local pi) ---
            let mut chat_ids: Vec<Uuid> = Vec::new();
            if let Some(chats) = sv.get("chats").and_then(|c| c.as_array()) {
                for cv in chats {
                    let Some(pid) = cv
                        .get("id")
                        .and_then(|x| x.as_str())
                        .and_then(|x| Uuid::parse_str(x).ok())
                    else {
                        continue;
                    };
                    // forge_sid null = local-pi backing (nil uuid)
                    let fsid = cv
                        .get("forge_sid")
                        .and_then(|x| x.as_str())
                        .and_then(|x| Uuid::parse_str(x).ok())
                        .unwrap_or_else(Uuid::nil);
                    let cwd = cv.get("cwd").and_then(|x| x.as_str()).map(String::from);
                    let pi_file = cv
                        .get("pi_session_file")
                        .and_then(|x| x.as_str())
                        .map(String::from);
                    s.chats.insert(
                        pid,
                        ChatPane {
                            forge_sid: fsid,
                            cols: 80,
                            rows: 24,
                            chat: vec![],
                            model: None,
                            context: None,
                            cwd: cwd.clone(),
                        }
                    );
                    chat_ids.push(pid);
                    let is_pi = fsid.is_nil();
                    if is_pi {
                        // respawn the local rpc child in the recorded cwd
                        let dir = cwd.clone().unwrap_or_else(home_dir_string);
                        let spawn_res = match &self.forge_pipe_w {
                            Some(pipe_w) => pilocal::LocalPi::spawn(
                                pid,
                                &dir,
                                pilocal::no_tools_configured(),
                                pipe_w.clone(),
                                &mut self.pi_agents,
                            ),
                            None => Err("no forge pipe".into()),
                        };
                        match spawn_res {
                            Ok(()) => {
                                eprintln!("ranchd: restored local pi pane {pid} in {dir}");
                                // recorded file may not exist (pi lazy-creates
                                // session files; an idle pane's path was never
                                // written) — then it's a fresh conversation, and
                                // switch_session below would only point pi at a
                                // path that doesn't exist yet
                                let restore_file =
                                    pilocal::LocalPi::resolve_restore_file(pi_file.as_deref());
                                if let (Some(rec), None) = (&pi_file, &restore_file) {
                                    eprintln!(
                                        "ranchd: pi restore: recorded {rec} was never written (no messages); fresh conversation"
                                    );
                                }
                                if let Some(sf) = &restore_file {
                                    // Give pi's RPC loop a moment to boot before
                                    // switching into the old session.  Retry a
                                    // few times because pi's init can be slow
                                    // on a loaded host.
                                    let mut switched = false;
                                    for attempt in 0..5u32 {
                                        std::thread::sleep(std::time::Duration::from_millis(400));
                                        if let Some(agent) = self.pi_agents.get(&pid) {
                                            match agent.switch_session(sf) {
                                                Ok(()) => {
                                                    eprintln!("ranchd: pi switch_session ok (attempt {}): {sf}", attempt + 1);
                                                    switched = true;
                                                    break;
                                                }
                                                Err(e) if attempt < 4 => {
                                                    eprintln!("ranchd: pi switch_session attempt {}/5 failed: {e} — retrying", attempt + 1);
                                                }
                                                Err(e) => {
                                                    eprintln!("ranchd: pi switch_session failed after retries: {e}");
                                                }
                                            }
                                        }
                                    }
                                    // Read the session file directly from disk to
                                    // populate chat history.  switch_session only
                                    // tells pi where to write; get_messages returns
                                    // empty because pi hasn't loaded history into
                                    // memory yet.  Note: the session isn't in
                                    // self.sessions yet — insert happens later —
                                    // so write to the local `s`. Done even when
                                    // the RPC switch failed: the rows are what the
                                    // user sees, and the next successful switch
                                    // (or fresh prompt) reconciles pi's side.
                                    if !switched {
                                        eprintln!("ranchd: pi history: loading rows from {sf} despite switch failure");
                                    }
                                    {
                                        match pilocal::LocalPi::read_session_messages(sf) {
                                            Ok(msgs) if !msgs.is_empty() => {
                                                eprintln!("ranchd: pi history: loaded {} rows from {sf}", msgs.len());
                                                if let Some(cp) = s.chats.get_mut(&pid) {
                                                    cp.chat = msgs;
                                                }
                                            }
                                            Ok(_) => {
                                                eprintln!("ranchd: pi history: session file {sf} had no messages");
                                            }
                                            Err(e) => {
                                                eprintln!("ranchd: pi history: failed to read {sf}: {e}");
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => eprintln!("ranchd: restore: pi respawn failed: {e}"),
                        }
                    } else if let Some(tx) = &self.forge_tx {
                        // forge-backed: re-subscribe the SSE watch (history
                        // replays from the forge session)
                        let _ = tx.send(forge::ForgeJob::Watch {
                            pane: pid,
                            forge_sid: fsid,
                        });
                        eprintln!("ranchd: restored forge chat pane {pid} -> {fsid}");
                    }
                }
            }

            // --- windows + pty panes ---
            if let Some(windows) = sv.get("windows").and_then(|w| w.as_array()) {
                for wv in windows {
                    let wname = wv
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("0")
                        .to_string();
                    // layout comes back verbatim; leaves referencing chat
                    // panes resolve against s.chats, pty leaves against
                    // freshly spawned panes below
                    let layout = wv
                        .get("layout")
                        .cloned()
                        .and_then(|l| serde_json::from_value::<Layout>(l).ok())
                        .unwrap_or_else(|| Layout::Leaf {
                            pane: Uuid::nil().to_string(),
                        });
                    s.windows.push(Window {
                        id: Uuid::new_v4(),
                        name: wname,
                        layout,
                    });
                }
            }
            if s.windows.is_empty() {
                s.windows.push(Window::new(
                    Layout::Leaf {
                        pane: Uuid::nil().to_string(),
                    },
                    "0".into(),
                ));
            }
            s.win = win_idx.min(s.windows.len() - 1);

            // spawn recorded pty panes (fresh shells in their last cwd;
            // running programs + scrollback are not restorable)
            if let Some(panes) = sv.get("panes").and_then(|p| p.as_array()) {
                for pv in panes {
                    let (Some(pid), Some(pkind)) = (
                        pv.get("id")
                            .and_then(|x| x.as_str())
                            .and_then(|x| Uuid::parse_str(x).ok()),
                        pv.get("kind").and_then(|x| x.as_str()).map(String::from),
                    ) else {
                        continue;
                    };
                    if pv.get("dead").and_then(|x| x.as_bool()).unwrap_or(false) {
                        continue;
                    }
                    if pkind == "forge-chat" || chat_ids.contains(&pid) {
                        continue; // chat panes already handled above
                    }
                    let cwd = pv.get("cwd").and_then(|x| x.as_str()).map(String::from);
                    // spawn_pane inserts into a Session and sizes from it;
                    // use a throwaway session then steal the pane under the
                    // RECORDED pane id so the layout's leaf ids stay valid
                    let mut s2 = Session {
                        id: Uuid::new_v4(),
                        name: String::new(),
                        kind: String::new(),
                        panes: BTreeMap::new(),
                        active: Uuid::nil(),
                        size: s.size,
                        windows: vec![Window::new(
                            Layout::Leaf {
                                pane: Uuid::nil().to_string(),
                            },
                            "0".into(),
                        )],
                        win: 0,
                        chats: BTreeMap::new(),
                    };
                    match spawn_pane(&mut s2, &pkind, cwd.as_deref()) {
                        Ok(new_pid) => {
                            if let Some(p) = s2.panes.remove(&new_pid) {
                                s.panes.insert(pid, p);
                            }
                        }
                        Err(e) => {
                            eprintln!("ranchd: restore: pane respawn failed: {e}");
                            // layout pruning below drops the dangling leaf
                        }
                    }
                }
            }

            // drop layout leaves that reference panes that failed to spawn
            for w in &mut s.windows {
                prune_layout(&mut w.layout, &s.panes, &s.chats);
            }
            // nil placeholder leaves don't count as live panes
            s.windows
                .retain(|w| w.pane_ids().iter().any(|u| !u.is_nil()));
            if s.windows.is_empty() {
                eprintln!("ranchd: restore: session {name} has no live panes, skipping");
                continue;
            }
            s.win = s.win.min(s.windows.len() - 1);
            // active pane: recorded value if live, else first pane of the
            // active window
            let recorded_active = sv
                .get("active")
                .and_then(|x| x.as_str())
                .and_then(|x| Uuid::parse_str(x).ok());
            let win_ids = s.win().pane_ids();
            s.active = match recorded_active {
                Some(a) if win_ids.contains(&a) => a,
                _ => win_ids.first().copied().unwrap_or_else(Uuid::nil),
            };
            // apply recorded sizes to the fresh PTYs
            s.apply_sizes();

            eprintln!(
                "ranchd: restored session {name} ({sid}) kind={kind} panes={} chats={}",
                s.panes.len(),
                s.chats.len()
            );
            self.sessions.insert(sid, s);
            self.mirror(relay::RelayOut::UpsertSession {
                id: sid.to_string(),
                name: name.clone(),
                kind: kind.clone(),
            });
            restored += 1;
        }
        if restored > 0 {
            eprintln!("ranchd: restore: {restored} session(s) rebuilt from state.json");
        }
        // upgrade state.json to the current format immediately (chats,
        // layouts, cwds) — don't wait for the first mutation
        self.write_state();
    }

    fn resolve_session(&self, ref_: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|(_, s)| s.name == ref_ || s.id.to_string() == ref_)
            .map(|(_, s)| s)
    }

    fn session_meta(&self) -> Vec<SessionMeta> {
        self.sessions
            .values()
            .map(|s| SessionMeta {
                id: s.id.to_string(),
                name: s.name.clone(),
                kind: s.kind.clone(),
                windows: s.windows.iter().map(|w| w.name.clone()).collect(),
                active_pane: s.active.to_string(),
                panes: s.panes.keys().map(|p| p.to_string()).collect(),
                ref_id: None,
            })
            .collect()
    }

    /// Fire-and-forget registry mirror op (no-op when relay disabled).
    fn mirror(&self, op: relay::RelayOut) {
        if let Some(tx) = &self.mirror_tx {
            let _ = tx.send(op);
        }
    }

    /// Deliver a full snapshot to every client attached to a session.
    /// Each recipient gets its own copy: clients that attached with a
    /// `chat_limit` receive truncated chat histories (their `Attach`
    /// frame carries it), the rest get the full conversation.
    fn resnap(&mut self, sid: &Uuid) {
        if let Some(s) = self.sessions.get(sid) {
            let recipients: Vec<RawFd> = self
                .clients
                .iter()
                .filter(|(_, c)| c.attach == Some(*sid))
                .map(|(f, _)| *f)
                .collect();
            for rfd in recipients {
                let limit = self.clients.get(&rfd).and_then(|c| c.chat_limit);
                if let Some(mut snap) = snapshot_session(s, limit) {
                    if let Frame::Snapshot { client, .. } = &mut snap {
                        *client = rfd.to_string();
                    }
                    if let Some(c) = self.clients.get_mut(&rfd) {
                        send_frame(c, &snap);
                    }
                }
            }
        }
    }

    fn handle_frame(&mut self, from: RawFd, frame: &Frame) {
        match frame {
            // hot upgrade: exec the same binary with --inherit — all
            // PTY/pi fds ride through the exec (never returns on success).
            // Relay clients are denied: hot upgrade lets the holder of a
            // machine key replace the running daemon binary, which a
            // shared/public account (e.g. the ranch demo) must not be able
            // to do. Only direct (unix-socket) clients may upgrade.
            Frame::Upgrade {} => {
                let relayed = self
                    .clients
                    .get(&from)
                    .is_some_and(|c| c.name == "relay");
                if relayed {
                    eprintln!("ranchd: upgrade denied: relay clients may not hot-upgrade");
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::Error {
                                req_id: None,
                                message: "upgrade denied: remote clients may not hot-upgrade the daemon".into(),
                            },
                        );
                    }
                    return;
                }
                eprintln!("ranchd: hot upgrade requested (client {from})");
                if let Err(e) = self.hot_upgrade() {
                    eprintln!("ranchd: hot upgrade failed: {e}");
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::Error {
                                req_id: None,
                                message: format!("hot upgrade failed: {e}"),
                            },
                        );
                    }
                }
                return;
            }
            // client -> forge: hand to the worker thread (blocking HTTP)
            Frame::ChatSend {
                session,
                pane,
                text,
                attachments,
                ..
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let Ok(pid) = Uuid::parse_str(pane) else {
                    return;
                };
                // Inline attached file content into the agent's prompt so
                // the agent can act on it without an extra read round-trip.
                let augmented = build_attached_prompt(&text, &attachments);
                if let Some(s) = self.sessions.get(&sid) {
                    if let Some(cp) = s.chats.get(&pid) {
                        if cp.forge_sid.is_nil() {
                            // local pi backing: prompt the child directly
                            if let Some(lp) = self.pi_agents.get(&pid) {
                                if let Some(pipe_w) = &self.forge_pipe_w {
                                    if let Err(e) = lp.prompt(pipe_w, &augmented, &text, &attachments) {
                                        eprintln!("ranchd: pi prompt failed: {e}");
                                    }
                                }
                            }
                        } else if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::Send {
                                pane: pid,
                                forge_sid: cp.forge_sid,
                                text: augmented,
                            });
                        }
                    }
                }
            }
            // ----- agent tools (Phase A): agents spawn/steer/close panes -----
            Frame::AgentSpawn {
                req_id,
                caller_pane,
                caller_session,
                kind,
                name,
                cwd,
                prompt,
                mode,
                callback,
                ..
            } => {
                self.handle_agent_spawn(
                    from,
                    req_id,
                    caller_pane,
                    caller_session,
                    kind,
                    name,
                    cwd,
                    prompt,
                    mode,
                    *callback,
                );
            }
            Frame::AgentSpawnApprove { spawn_id, allow } => {
                self.handle_agent_approve(from, spawn_id.clone(), *allow);
            }
            Frame::AgentAsk {
                req_id,
                caller_pane,
                question,
                choices,
                suggested,
                multi,
                free_text,
            } => {
                self.handle_agent_ask(
                    from,
                    req_id,
                    caller_pane,
                    question,
                    choices.clone(),
                    *suggested,
                    *multi,
                    *free_text,
                );
            }
            Frame::AgentAskAnswer { ask_id, choices, text } => {
                self.handle_agent_ask_answer(ask_id, choices.clone(), text);
            }
            Frame::AgentAskStatus {
                req_id,
                caller_pane,
                ask_id,
            } => {
                self.handle_agent_ask_status(
                    from,
                    req_id,
                    caller_pane,
                    ask_id,
                );
            }
            Frame::AgentSend {
                req_id,
                caller_pane,
                session,
                pane,
                text,
                delivery,
            } => {
                self.handle_agent_send(from, req_id, caller_pane, session, pane, text, delivery);
            }
            Frame::AgentStatus {
                req_id,
                pane,
                ..
            } => {
                let Ok(pid) = Uuid::parse_str(pane) else { return };
                let info = self.sessions.values().find_map(|s| {
                    s.chats.get(&pid).map(|cp| (s.id, cp.model.clone()))
                });
                // status is read-only and non-sensitive; allow
                let (state, model) = match &info {
                    Some((_, model)) => {
                        // busy = last row is a user row (snapshot heuristic)
                        let busy = self.sessions.values().find_map(|s| {
                            s.chats.get(&pid).map(|cp| {
                                cp.chat.last().is_some_and(|l| l.role == "user")
                            })
                        });
                        (
                            match busy {
                                Some(true) => "working",
                                Some(false) => "idle",
                                None => "unknown",
                            }
                            .to_string(),
                            model.clone(),
                        )
                    }
                    None => ("unknown".to_string(), None),
                };
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::AgentStatusOk {
                            req_id: req_id.clone(),
                            pane: pid.to_string(),
                            state,
                            model,
                        },
                    );
                }
            }
            Frame::AgentRead {
                req_id,
                caller_pane,
                pane,
                since_seq,
                limit,
            } => {
                let Ok(pid) = Uuid::parse_str(pane) else { return };
                let caller = Self::frame_caller(caller_pane, req_id);
                let authorized = caller.is_nil() || self.spawns.authorized(caller, pid);
                let msgs = if !authorized {
                    None
                } else {
                    self.sessions.values().find_map(|s| {
                        s.chats.get(&pid).map(|cp| {
                            cp.chat
                                .iter()
                                .filter(|m| m.seq > *since_seq)
                                .rev()
                                .take(*limit as usize)
                                .cloned()
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                        })
                    })
                };
                if let Some(c) = self.clients.get_mut(&from) {
                    match msgs {
                        Some(msgs) => {
                            send_frame(
                                c,
                                &Frame::AgentReadOk {
                                    req_id: req_id.clone(),
                                    pane: pid.to_string(),
                                    msgs,
                                },
                            );
                        }
                        None => send_frame(
                            c,
                            &Frame::Error {
                                req_id: Some(req_id.clone()),
                                message: format!("pane {pane} not found or not yours"),
                            },
                        ),
                    }
                }
            }
            Frame::AgentClose {
                req_id,
                caller_pane,
                session,
                pane,
            } => {
                self.handle_agent_close(from, req_id, caller_pane, session, pane);
            }
            // client -> agent: list the models a chat pane can run on
            Frame::ModelList { pane, req_id, .. } => {
                let Ok(pid) = Uuid::parse_str(pane) else {
                    return;
                };
                let backing = self.sessions.iter().find_map(|(_, s)| {
                    s.chats.get(&pid).map(|cp| cp.forge_sid)
                });
                match backing {
                    Some(fsid) if fsid.is_nil() => {
                        // local pi pane: ask the rpc child (the reader
                        // thread answers with ModelListOk)
                        match self.pi_agents.get(&pid) {
                            Some(lp) => {
                                if let Err(e) = lp.request_model_list(req_id) {
                                    eprintln!("ranchd: pi model list failed: {e}");
                                }
                            }
                            None => {
                                if let Some(c) = self.clients.get_mut(&from) {
                                    send_frame(
                                        c,
                                        &Frame::Error {
                                            req_id: Some(req_id.clone()),
                                            message: format!("no agent for pane {pane}"),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    Some(forge_sid) => {
                        if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::ModelList {
                                pane: pid,
                                forge_sid,
                                req_id: req_id.clone(),
                            });
                        }
                    }
                    None => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: format!("not a chat pane: {pane}"),
                                },
                            );
                        }
                    }
                }
            }
            // client -> agent: switch a chat pane's model. Success is
            // confirmed out-of-band (`meta kind="model"`); sync failures
            // answer with `error { req_id }`.
            Frame::ModelSet {
                session,
                pane,
                provider,
                model,
                req_id,
                ..
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let Ok(pid) = Uuid::parse_str(pane) else {
                    return;
                };
                let backing = self
                    .sessions
                    .get(&sid)
                    .and_then(|s| s.chats.get(&pid))
                    .map(|cp| cp.forge_sid);
                match backing {
                    Some(fs) => {
                        if fs.is_nil() {
                            match self.pi_agents.get(&pid) {
                                Some(lp) => {
                                    if let Err(e) = lp.set_model(provider, model, req_id) {
                                        eprintln!("ranchd: pi set_model failed: {e}");
                                        if let Some(c) = self.clients.get_mut(&from) {
                                            send_frame(
                                                c,
                                                &Frame::Error {
                                                    req_id: Some(req_id.clone()),
                                                    message: e,
                                                },
                                            );
                                        }
                                    }
                                }
                                None => {
                                    if let Some(c) = self.clients.get_mut(&from) {
                                        send_frame(
                                            c,
                                            &Frame::Error {
                                                req_id: Some(req_id.clone()),
                                                message: format!("no agent for pane {pane}"),
                                            },
                                        );
                                    }
                                }
                            }
                        } else if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::ModelSet {
                                pane: pid,
                                forge_sid: fs,
                                req_id: req_id.clone(),
                                provider: provider.clone(),
                                model: model.clone(),
                            });
                        }
                    }
                    None => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: format!("not a chat pane: {pane}"),
                                },
                            );
                        }
                    }
                }
            }
            // client -> agent: manually compact a chat pane's context.
            // Success is confirmed out-of-band (meta kind="context");
            // sync failures answer with `error { req_id }`.
            Frame::ChatCompact {
                session,
                pane,
                req_id,
                ..
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let Ok(pid) = Uuid::parse_str(pane) else {
                    return;
                };
                let backing = self
                    .sessions
                    .get(&sid)
                    .and_then(|s| s.chats.get(&pid))
                    .map(|cp| cp.forge_sid);
                match backing {
                    Some(fs) => {
                        // machine-wide lifecycle: every client sees compaction
                        // start (the phone keys its in-progress UI off this;
                        // completion clears it via the context "compacted"
                        // readout and the agent-idle that follows)
                        let compacting = Frame::Meta {
                            session: sid.to_string(),
                            pane: Some(pid.to_string()),
                            kind: "agent".into(),
                            status: Some("compacting".into()),
                        };
                        let fds: Vec<RawFd> = self.clients.keys().copied().collect();
                        for fd in fds {
                            if let Some(c) = self.clients.get_mut(&fd) {
                                send_frame(c, &compacting);
                            }
                        }
                        if fs.is_nil() {
                            match self.pi_agents.get(&pid) {
                                Some(lp) => {
                                    if let Err(e) = lp.compact(req_id) {
                                        eprintln!("ranchd: pi compact failed: {e}");
                                        if let Some(c) = self.clients.get_mut(&from) {
                                            send_frame(
                                                c,
                                                &Frame::Error {
                                                    req_id: Some(req_id.clone()),
                                                    message: e,
                                                },
                                            );
                                        }
                                    }
                                }
                                None => {
                                    if let Some(c) = self.clients.get_mut(&from) {
                                        send_frame(
                                            c,
                                            &Frame::Error {
                                                req_id: Some(req_id.clone()),
                                                message: format!("no agent for pane {pane}"),
                                            },
                                        );
                                    }
                                }
                            }
                        } else if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::Compact {
                                pane: pid,
                                forge_sid: fs,
                                req_id: req_id.clone(),
                            });
                        }
                    }
                    None => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: format!("not a chat pane: {pane}"),
                                },
                            );
                        }
                    }
                }
            }
            // forge worker -> clients: broadcast new chat rows to
            // everyone attached to the session (pipe client has no
            // attach; the frame carries the session in `session` —
            // the worker leaves it blank, so resolve from the pane)
            // client -> daemon: local directory listing (cheap, sync)
            Frame::DirList { req_id, path, .. } => {
                let base = path
                    .clone()
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
                let (dirs, files, parent) = match std::fs::read_dir(&base) {
                    Ok(rd) => {
                        let mut ds: Vec<String> = Vec::new();
                        let mut fs: Vec<String> = Vec::new();
                        for e in rd.flatten() {
                            let name = e.file_name().to_string_lossy().to_string();
                            if name.starts_with('.') {
                                continue;
                            }
                            match e.file_type() {
                                Ok(t) if t.is_dir() => ds.push(name),
                                Ok(t) if t.is_file() => fs.push(name),
                                _ => {}
                            }
                        }
                        ds.sort();
                        fs.sort();
                        let parent = std::path::Path::new(&base)
                            .parent()
                            .map(|p| p.to_string_lossy().to_string());
                        (ds, fs, parent)
                    }
                    Err(e) => {
                        eprintln!("ranchd: dir list {base}: {e}");
                        (Vec::new(), Vec::new(), None)
                    }
                };
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::DirListOk {
                            id: String::new(),
                            req_id: req_id.clone(),
                            path: base,
                            parent,
                            dirs,
                            files,
                        },
                    );
                }
            }
            // client -> daemon: read a file's contents (M10 editor)
            Frame::FileRead { req_id, path, .. } => {
                let file = std::path::Path::new(path);
                let mut reply_err = |msg: String| {
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::Error {
                                req_id: Some(req_id.clone()),
                                message: msg,
                            },
                        );
                    }
                };
                match std::fs::metadata(file) {
                    Ok(meta) => {
                        if !meta.is_file() {
                            reply_err(format!("{path} is not a regular file"));
                            return;
                        }
                        if meta.len() > FILE_MAX_BYTES {
                            reply_err(format!(
                                "file too large to edit ({0} bytes; limit {1})",
                                meta.len(),
                                FILE_MAX_BYTES
                            ));
                            return;
                        }
                        let content = match std::fs::read_to_string(file) {
                            Ok(c) => c,
                            Err(e) => {
                                reply_err(format!("could not read {path} as text: {e}"));
                                return;
                            }
                        };
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        if let Some(c) = self.clients.get_mut(&from) {
                            // M10 ph3: auto-watch for external edits;
                            // baseline = the mtime we just read
                            c.file_watches.insert(path.clone(), mtime);
                            send_frame(
                                c,
                                &Frame::FileReadOk {
                                    id: String::new(),
                                    req_id: req_id.clone(),
                                    path: path.clone(),
                                    content,
                                    mtime,
                                    size: meta.len(),
                                },
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("ranchd: file read {path}: {e}");
                        reply_err(format!("could not read {path}: {e}"));
                    }
                }
            }
            // client -> daemon: write a file's contents atomically (M10 editor).
            // Conflict check: if the client supplied the mtime it read, refuse
            // to clobber when the file has since changed on disk.
            Frame::FileWrite {
                req_id,
                path,
                content,
                mtime,
                ..
            } => {
                let file = std::path::Path::new(path);
                let mut reply_err = |msg: String| {
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::Error {
                                req_id: Some(req_id.clone()),
                                message: msg,
                            },
                        );
                    }
                };
                if let Some(expected) = mtime {
                    match std::fs::metadata(file) {
                        Ok(meta) => {
                            let cur = meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            if cur != 0 && cur != *expected {
                                reply_err(format!(
                                    "file changed on disk (mtime {cur}, you read {expected}) — reload before saving"
                                ));
                                return;
                            }
                        }
                        Err(_) => {}
                    }
                }
                let parent = file
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                if !parent.is_dir() {
                    reply_err(format!(
                        "parent directory does not exist: {}",
                        parent.display()
                    ));
                    return;
                }
                let tmp = parent.join(format!(
                    ".{}.ranch-tmp",
                    file.file_name().and_then(|n| n.to_str()).unwrap_or("file")
                ));
                match std::fs::write(&tmp, &content) {
                    Ok(()) => match std::fs::rename(&tmp, file) {
                        Ok(()) => {
                            let mtime = std::fs::metadata(file)
                                .and_then(|m| m.modified())
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            if let Some(c) = self.clients.get_mut(&from) {
                                // our own write moved the mtime; refresh
                                // the watch baseline so the tick doesn't
                                // push a spurious FileChanged
                                c.file_watches.insert(path.clone(), mtime);
                                send_frame(
                                    c,
                                    &Frame::FileWriteOk {
                                        id: String::new(),
                                        req_id: req_id.clone(),
                                        path: path.clone(),
                                        mtime,
                                    },
                                );
                            }
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp);
                            reply_err(format!("could not write {path}: {e}"));
                        }
                    },
                    Err(e) => {
                        reply_err(format!("could not write {path}: {e}"));
                    }
                }
            }
            // client -> daemon: upload a file from a remote client (mobile)
            // into the daemon's uploads dir; reply with the absolute path.
            Frame::FilePut {
                req_id,
                name,
                b64,
                ..
            } => {
                use base64::Engine as _;
                let dec = base64::engine::general_purpose::STANDARD;
                eprintln!("ranchd: FilePut from client {from}: name={name} b64_len={}", b64.len());
                let mut reply_err = |msg: String| {
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::Error {
                                req_id: Some(req_id.clone()),
                                message: msg,
                            },
                        );
                    }
                };
                // base64-decode (cap decoded size to 50 MiB to avoid a
                // memory-blow from a malicious/accidental huge upload)
                let bytes = match dec.decode(b64.as_bytes()) {
                    Ok(b) => b,
                    Err(e) => {
                        reply_err(format!("invalid base64: {e}"));
                        return;
                    }
                };
                if bytes.len() > 10 * 1024 * 1024 {
                    reply_err("file too large (max 10 MiB)".into());
                    return;
                }
                // sanitize the suggested name: take the basename, strip
                // path separators / traversal, fall back to a generated name
                let clean: String = name
                    .rsplit('/')
                    .next()
                    .unwrap_or(name.as_str())
                    .trim()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
                    .collect();
                let clean = if clean.len() > 64 {
                    clean.get(..64).unwrap_or(clean.as_str()).to_string()
                } else if clean.is_empty() {
                    "upload".to_string()
                } else {
                    clean
                };
                let uploads = home_dir()
                    .join(".local/state/ranch/uploads");
                if let Err(e) = std::fs::create_dir_all(&uploads) {
                    reply_err(format!("cannot create uploads dir: {e}"));
                    return;
                }
                let ts = chrono::Utc::now().timestamp();
                let dest = uploads.join(format!("{ts}-{clean}"));
                match std::fs::write(&dest, &bytes) {
                    Ok(()) => {
                        eprintln!("ranchd: FilePut stored {} ({} bytes) at {}", name, bytes.len(), dest.display());
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::FilePutOk {
                                    id: String::new(),
                                    req_id: req_id.clone(),
                                    path: dest.to_string_lossy().into_owned(),
                                    size: bytes.len() as u64,
                                },
                            );
                        }
                    }
                    Err(e) => reply_err(format!("could not write upload: {e}")),
                }
            }
            // client -> forge: list resumable sessions (blocking HTTP
            // on the worker thread)
            Frame::ForgeList { req_id, .. } => {
                if let Some(tx) = &self.forge_tx {
                    let _ = tx.send(forge::ForgeJob::List {
                        req_id: req_id.clone(),
                    });
                } else if let Some(c) = self.clients.get_mut(&from) {
                    // forge not configured: empty reply (no error — the
                    // client just shows nothing to resume)
                    send_frame(
                        c,
                        &Frame::ForgeListOk {
                            id: String::new(),
                            req_id: req_id.clone(),
                            sessions: Vec::new(),
                        },
                    );
                }
            }
            // client -> daemon: list resumable local pi sessions
            Frame::PiList { req_id, .. } => {
                // Collect all ranch-tracked pi session file paths
                let mut known_files: std::collections::HashSet<String> = std::collections::HashSet::new();
                for (_, s) in &self.sessions {
                    for (_pid, cp) in &s.chats {
                        if cp.forge_sid.is_nil() {
                            if let Some(a) = self.pi_agents.get(&_pid) {
                                if let Ok(g) = a.session_file.lock() {
                                    if let Some(sf) = g.clone() {
                                        known_files.insert(sf);
                                    }
                                }
                            }
                        }
                    }
                }
                // Live pi agents
                let mut sessions: Vec<PiSessionInfo> = self.pi_agents.iter().map(|(pid, lp)| {
                    let cwd = lp.cwd.clone();
                    let session_file = lp.session_file.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
                    // Prefer a title derived from the last user message; fall
                    // back to the working directory while the session file
                    // hasn't been created yet (pi writes it lazily).
                    let (title, updated) = if session_file.is_empty() {
                        (cwd.clone(), String::new())
                    } else {
                        (
                            pi_session_meta(&session_file).1.unwrap_or(cwd.clone()),
                            file_mtime_secs(&session_file),
                        )
                    };
                    PiSessionInfo {
                        id: pid.to_string(),
                        title,
                        path: cwd,
                        session_file,
                        active: true,
                        external: false,
                        updated,
                    }
                }).collect();
                // External pi sessions: scan ~/.pi/agent/sessions/ when enabled
                if self.monitor_external_pi {
                    let sessions_dir = home_dir().join(".pi/agent/sessions");
                    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
                        for entry in entries.flatten() {
                            let sub_dir = entry.path();
                            if !sub_dir.is_dir() { continue; }
                            if let Ok(files) = std::fs::read_dir(&sub_dir) {
                                for file_entry in files.flatten() {
                                    let fpath = file_entry.path();
                                    if fpath.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
                                    let path_str = fpath.to_string_lossy().to_string();
                                    if known_files.contains(&path_str) { continue; }
                                    // (id, last-user-message title, cwd) from
                                    // the session file; falls back below.
                                    let (sid, title, cwd) = pi_session_meta(&path_str);
                                    // mtime as unix timestamp string
                                    let mtime = file_mtime_secs(&path_str);
                                    // Use directory name as a path fallback;
                                    // prefer the last user message as the title.
                                    let dir_name = sub_dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
                                    sessions.push(PiSessionInfo {
                                        id: sid.unwrap_or(path_str.clone()),
                                        title: title.filter(|t| !t.is_empty()).unwrap_or(dir_name.clone()),
                                        path: cwd.filter(|c| !c.is_empty()).unwrap_or(dir_name.clone()),
                                        session_file: path_str,
                                        active: false,
                                        external: true,
                                        updated: mtime,
                                    });
                                }
                            }
                        }
                    }
                }
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::PiListOk {
                            id: String::new(),
                            req_id: req_id.clone(),
                            sessions,
                        },
                    );
                }
            }
            // client -> daemon: toggle external pi monitoring
            Frame::PiMonitor { enabled, req_id } => {
                self.monitor_external_pi = *enabled;
                self.write_state();
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::PiMonitorOk {
                            req_id: req_id.clone(),
                            enabled: self.monitor_external_pi,
                        },
                    );
                }
            }
            // ----- agent builder (Phase B): profile CRUD proxy -----
            Frame::ModelCatalog { req_id } => {
                match &self.forge_tx {
                    Some(tx) => {
                        let _ = tx.send(forge::ForgeJob::ModelCatalog { req_id: req_id.clone() });
                    }
                    None => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            // no forge: empty catalog (clients fall back
                            // to a free-text model field)
                            send_frame(
                                c,
                                &Frame::ModelCatalogOk {
                                    req_id: req_id.clone(),
                                    models: Vec::new(),
                                },
                            );
                        }
                    }
                }
            }
            Frame::ProfileList { req_id } => {
                match &self.forge_tx {
                    Some(tx) => {
                        let _ = tx.send(forge::ForgeJob::ProfileList { req_id: req_id.clone() });
                    }
                    None => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: "forge not configured".into(),
                                },
                            );
                        }
                    }
                }
            }
            Frame::ProfileGet { req_id, profile } => {
                match &self.forge_tx {
                    Some(tx) => {
                        let _ = tx.send(forge::ForgeJob::ProfileGet {
                            req_id: req_id.clone(),
                            profile_id: profile.clone(),
                        });
                    }
                    None => Self::forge_unconfigured(self, from, req_id),
                }
            }
            Frame::ProfilePut {
                req_id,
                profile_id,
                draft,
            } => {
                match &self.forge_tx {
                    Some(tx) => {
                        let _ = tx.send(forge::ForgeJob::ProfilePut {
                            req_id: req_id.clone(),
                            profile_id: profile_id.clone(),
                            draft: draft.clone(),
                        });
                    }
                    None => Self::forge_unconfigured(self, from, req_id),
                }
            }
            Frame::ProfileDelete { req_id, profile } => {
                match &self.forge_tx {
                    Some(tx) => {
                        let _ = tx.send(forge::ForgeJob::ProfileDelete {
                            req_id: req_id.clone(),
                            profile_id: profile.clone(),
                        });
                    }
                    None => Self::forge_unconfigured(self, from, req_id),
                }
            }
            // ----- triggers (Phase D) -----
            Frame::TriggerList { req_id } => {
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::TriggerListOk {
                            req_id: req_id.clone(),
                            triggers: self
                                .triggers
                                .triggers
                                .values()
                                .map(|t| {
                                    serde_json::json!({
                                        "id": t.id.to_string(),
                                        "name": t.name,
                                        "workflow_id": t.workflow_id,
                                        "kind": t.kind_tag(),
                                        "spec": t.spec_value(),
                                        "input": t.input,
                                        "enabled": t.enabled,
                                        "catch_up": t.catch_up,
                                        "last_run": t.last_run.as_ref().map(|(j, ts, st)| serde_json::json!({
                                            "job": j, "at": ts, "status": st,
                                        })),
                                    })
                                })
                                .collect(),
                        },
                    );
                }
            }
            Frame::TriggerPut {
                req_id,
                trigger_id,
                trigger,
            } => {
                // parse the trigger shape; persist; mirror
                let res = (|| {
                    let name = trigger
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        return Err("trigger name is required".into());
                    }
                    let workflow_id = trigger
                        .get("workflow_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if workflow_id.is_empty() {
                        return Err("trigger workflow_id is required".into());
                    }
                    let kind_tag = trigger.get("kind").and_then(|x| x.as_str()).unwrap_or("");
                    let spec = trigger.get("spec").cloned().unwrap_or(serde_json::Value::Null);
                    let kind = match kind_tag {
                        "cron" => {
                            let expr = spec.get("cron").and_then(|x| x.as_str()).unwrap_or("");
                            if triggers::cron_next(expr, triggers::Scheduler::now()).is_none() {
                                return Err(format!("bad cron expression {expr:?}"));
                            }
                            triggers::TriggerKind::Cron { expr: expr.into() }
                        }
                        "event" => triggers::TriggerKind::Event {
                            event: spec.get("event").and_then(|x| x.as_str()).unwrap_or("").into(),
                            filter: spec.get("filter").cloned().unwrap_or(serde_json::Value::Null),
                        },
                        "webhook" => triggers::TriggerKind::Webhook {
                            source: spec.get("source").and_then(|x| x.as_str()).unwrap_or("").into(),
                            event_tag: spec.get("event").and_then(|x| x.as_str()).map(String::from),
                        },
                        other => return Err(format!("unknown trigger kind {other:?} (cron|event|webhook)")),
                    };
                    Ok(triggers::Trigger {
                        id: trigger_id
                            .as_deref()
                            .and_then(|s| Uuid::parse_str(s).ok())
                            .unwrap_or_else(Uuid::new_v4),
                        name,
                        workflow_id,
                        kind,
                        input: trigger.get("input").cloned().filter(|x| !x.is_null()),
                        enabled: trigger.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
                        catch_up: trigger.get("catch_up").and_then(|x| x.as_bool()).unwrap_or(false),
                        last_run: None,
                    })
                })();
                match res {
                    Ok(t) => {
                        let mirror = crate::relay::RelayOut::UpsertTrigger {
                            id: t.id.to_string(),
                            name: t.name.clone(),
                            workflow_id: t.workflow_id.clone(),
                            kind: t.kind_tag().into(),
                            enabled: t.enabled,
                            spec: t.spec_value(),
                        };
                        let id = t.id.to_string();
                        self.triggers.triggers.insert(t.id, t);
                        self.write_state();
                        self.mirror(mirror);
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(c, &Frame::TriggerPutOk { req_id: req_id.clone(), trigger_id: id });
                        }
                    }
                    Err(e) => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: e,
                                },
                            );
                        }
                    }
                }
            }
            Frame::TriggerDelete { req_id, trigger } => {
                let parsed = Uuid::parse_str(trigger);
                if let Ok(id) = parsed {
                    self.triggers.triggers.remove(&id);
                    self.write_state();
                    self.mirror(crate::relay::RelayOut::DeleteTrigger { id: id.to_string() });
                }
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(c, &Frame::TriggerDeleteOk { req_id: req_id.clone() });
                }
            }
            Frame::TriggerRun { req_id, trigger } => {
                let id = Uuid::parse_str(trigger).ok();
                let fireable = id.and_then(|id| self.triggers.triggers.get(&id).cloned());
                match fireable {
                    Some(t) if self.mule_tx.is_some() => {
                        let job_id = Uuid::new_v4().to_string();
                        let _ = self.mule_tx.as_ref().unwrap().send(mule::MuleJob::Run {
                            req_id: Uuid::new_v4().to_string(),
                            workflow_id: t.workflow_id.clone(),
                            input: t.input.clone(),
                            session: Uuid::nil(),
                            pane: Uuid::nil(),
                        });
                        if let Some(t) = self.triggers.triggers.get_mut(&t.id) {
                            t.last_run = Some((job_id.clone(), triggers::Scheduler::now(), "queued".into()));
                        }
                        self.write_state();
                        let fired = Frame::TriggerFired {
                            trigger: id.unwrap().to_string(),
                            job: job_id,
                        };
                        let fds: Vec<RawFd> = self.clients.keys().copied().collect();
                        for fd in fds {
                            if let Some(c) = self.clients.get_mut(&fd) {
                                send_frame(c, &fired);
                            }
                        }
                    }
                    _ => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: if self.mule_tx.is_none() {
                                        "mule not configured".into()
                                    } else {
                                        format!("trigger {trigger:?} not found")
                                    },
                                },
                            );
                        }
                    }
                }
            }

            // ----- webhooks (Phase E) -----
            Frame::WebhookEvent {
                webhook,
                source,
                event,
                payload,
                ..
            } => {
                // Only arrives via the relay pipe (edge function). Match
                // against webhook triggers and fire workflows. The
                // payload nests under "payload" for trigger templates.
                let event_type = event.as_deref().unwrap_or("");
                let (tframes, tmirrors) = self.triggers.fire_event(
                    "webhook",
                    &serde_json::json!({
                        "source": source,
                        "event": event_type,
                        "payload": payload,
                    }),
                );
                for m in tmirrors {
                    self.mirror(m);
                }
                let fired_n = tframes.len();
                for f in tframes {
                    let fds: Vec<RawFd> = self.clients.keys().copied().collect();
                    for fd in fds {
                        if let Some(c) = self.clients.get_mut(&fd) {
                            send_frame(c, &f);
                        }
                    }
                }
                eprintln!(
                    "ranchd: webhook {source}/{event_type} from {webhook} ({fired_n} triggers fired)"
                );
            }
            Frame::WebhookList { req_id } => {
                // REST via the relay's REST helpers requires the relay
                // config; reply unconfigured when absent
                let Some(rcfg) = relay::load_config() else {
                    Self::relay_unconfigured(self, from, req_id);
                    return;
                };
                let jwt = relay::machine_jwt_blocking(&rcfg);
                let url = format!(
                    "{}/rest/v1/webhooks?select=id,name,sources,enabled,created_at&machine_id=eq.{}",
                    rcfg.supabase_url, rcfg.machine_id
                );
                match relay::rest_get(&rcfg, &jwt, &url) {
                    Ok(v) => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::WebhookListOk {
                                    req_id: req_id.clone(),
                                    webhooks: v.as_array().cloned().unwrap_or_default(),
                                },
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: e,
                                },
                            );
                        }
                    }
                }
            }
            Frame::WebhookPut {
                req_id,
                webhook_id: _,
                name,
                sources,
            } => {
                let Some(rcfg) = relay::load_config() else {
                    Self::relay_unconfigured(self, from, req_id);
                    return;
                };
                let jwt = relay::machine_jwt_blocking(&rcfg);
                // create: generate the secret here (never from the client),
                // store it encrypted via the store_webhook_secret RPC
                let new_id = Uuid::new_v4();
                let raw_secret = format!("whsec_{}", Uuid::new_v4().simple());
                let body = serde_json::json!({
                    "id": new_id.to_string(),
                    "machine_id": rcfg.machine_id,
                    "name": name,
                    "sources": sources,
                    "enabled": true,
                });
                let url = format!("{}/rest/v1/webhooks", rcfg.supabase_url);
                match relay::rest_post(&rcfg, &jwt, &url, &body) {
                    Ok(_) => {
                        // encrypt + store the secret (security-definer RPC)
                        let _ = relay::rest_post(
                            &rcfg,
                            &jwt,
                            &format!("{}/rest/v1/rpc/store_webhook_secret", rcfg.supabase_url),
                            &serde_json::json!({
                                "p_webhook_id": new_id.to_string(),
                                "p_secret": raw_secret,
                            }),
                        );
                        let url = format!(
                            "{}/functions/v1/webhook-relay/w/{}",
                            rcfg.supabase_url.replace(".supabase.co", ".supabase.co"),
                            new_id
                        );
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::WebhookPutOk {
                                    req_id: req_id.clone(),
                                    webhook_id: new_id.to_string(),
                                    url,
                                    secret: Some(raw_secret), // shown ONCE
                                },
                            );
                        }
                        self.mirror(crate::relay::RelayOut::UpsertWebhook {
                            id: new_id.to_string(),
                            name: name.clone(),
                            sources: sources.clone(),
                            enabled: true,
                        });
                    }
                    Err(e) => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: e,
                                },
                            );
                        }
                    }
                }
            }
            Frame::WebhookDelete { req_id, webhook } => {
                let Some(rcfg) = relay::load_config() else {
                    Self::relay_unconfigured(self, from, req_id);
                    return;
                };
                let jwt = relay::machine_jwt_blocking(&rcfg);
                let url = format!(
                    "{}/rest/v1/webhooks?id=eq.{webhook}&machine_id=eq.{}",
                    rcfg.supabase_url, rcfg.machine_id
                );
                match relay::rest_delete(&rcfg, &jwt, &url) {
                    Ok(_) => {
                        self.mirror(crate::relay::RelayOut::DeleteWebhook {
                            id: webhook.clone(),
                        });
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(c, &Frame::WebhookDeleteOk { req_id: req_id.clone() });
                        }
                    }
                    Err(e) => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: e,
                                },
                            );
                        }
                    }
                }
            }

            // ----- workflows (Phase C): mule proxy -----
            Frame::MuleAgents { req_id } => {
                match &self.mule_tx {
                    Some(tx) => {
                        let _ = tx.send(mule::MuleJob::Agents { req_id: req_id.clone() });
                    }
                    None => Self::mule_unconfigured(self, from, req_id),
                }
            }
            Frame::WorkflowList { req_id } => {
                match &self.mule_tx {
                    Some(tx) => {
                        let _ = tx.send(mule::MuleJob::List { req_id: req_id.clone() });
                    }
                    None => Self::mule_unconfigured(self, from, req_id),
                }
            }
            Frame::WorkflowGet { req_id, workflow } => {
                match &self.mule_tx {
                    Some(tx) => {
                        let _ = tx.send(mule::MuleJob::Get {
                            req_id: req_id.clone(),
                            workflow_id: workflow.clone(),
                        });
                    }
                    None => Self::mule_unconfigured(self, from, req_id),
                }
            }
            Frame::WorkflowPut {
                req_id,
                workflow_id,
                draft,
            } => {
                match &self.mule_tx {
                    Some(tx) => {
                        let _ = tx.send(mule::MuleJob::Put {
                            req_id: req_id.clone(),
                            workflow_id: workflow_id.clone(),
                            draft: draft.clone(),
                        });
                    }
                    None => Self::mule_unconfigured(self, from, req_id),
                }
            }
            Frame::WorkflowDelete { req_id, workflow } => {
                match &self.mule_tx {
                    Some(tx) => {
                        let _ = tx.send(mule::MuleJob::Delete {
                            req_id: req_id.clone(),
                            workflow_id: workflow.clone(),
                        });
                    }
                    None => Self::mule_unconfigured(self, from, req_id),
                }
            }
            Frame::WorkflowRun {
                req_id,
                workflow,
                input,
            } => {
                let Some(tx) = &self.mule_tx else {
                    Self::mule_unconfigured(self, from, req_id);
                    return;
                };
                // the run pane: a dedicated chat pane (no backing agent) in a
                // session named after the workflow — watchable from any client
                let wf_name = self
                    .resolve_session(workflow)
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| "workflow".into());
                let sid = Uuid::new_v4();
                let pid = Uuid::new_v4();
                let mut sess = Session {
                    id: sid,
                    name: format!("wf-{}", &workflow[..8.min(workflow.len())]),
                    kind: "mule".into(),
                    panes: BTreeMap::new(),
                    active: Uuid::nil(),
                    size: (80, 24),
                    windows: vec![Window::new(
                        Layout::Leaf { pane: pid.to_string() },
                        "0".into(),
                    )],
                    win: 0,
                    chats: BTreeMap::new(),
                };
                sess.chats.insert(
                    pid,
                    ChatPane {
                        forge_sid: Uuid::nil(), // no agent backing: rows come from the mule worker
                        cols: 80,
                        rows: 24,
                        chat: vec![],
                        model: None,
                        context: None,
                        cwd: None,
                    }
                );
                sess.active = pid;
                self.sessions.insert(sid, sess);
                self.write_state();
                self.mirror(relay::RelayOut::UpsertSession {
                    id: sid.to_string(),
                    name: format!("wf-{}", &workflow[..8.min(workflow.len())]),
                    kind: "mule".into(),
                });
                let _ = tx.send(mule::MuleJob::Run {
                    req_id: req_id.clone(),
                    workflow_id: workflow.clone(),
                    input: input.clone(),
                    session: sid,
                    pane: pid,
                });
                let _ = wf_name;
            }
            // forge worker agent-status: resolve + broadcast
            Frame::Meta {
                session,
                pane,
                kind,
                status,
            } if session.is_empty()
                && (kind == "agent" || kind == "model" || kind == "workflow"
                    || kind == "context") =>
            {
                let pid = Uuid::parse_str(pane.as_deref().unwrap_or("")).ok();
                let found = pid.and_then(|pid| {
                    self.sessions
                        .iter()
                        .find_map(|(sid, s)| s.chats.contains_key(&pid).then_some((*sid, pid)))
                });
                if let Some((sid, pid)) = found {
                    // model frames double as the pane cache update so
                    // re-snapshots carry the current model
                    if kind == "model" {
                        if let Some(st) = status {
                            if let Some(s) = self.sessions.get_mut(&sid) {
                                if let Some(cp) = s.chats.get_mut(&pid) {
                                    cp.model = Some(st.clone());
                                }
                            }
                        }
                    }
                    // context readouts cache like model: re-snapshots carry
                    // the last known usage so attaching clients see it
                    if kind == "context" {
                        if let Some(st) = status {
                            if let Some(s) = self.sessions.get_mut(&sid) {
                                if let Some(cp) = s.chats.get_mut(&pid) {
                                    cp.context = Some(st.clone());
                                }
                            }
                        }
                    }
                    // Phase D: event triggers see every agent turn end
                    if kind == "agent" && status.as_deref() == Some("idle") {
                        let (tframes, tmirrors) = self.triggers.fire_event(
                            "agent_turn_ended",
                            &serde_json::json!({ "pane": pid.to_string(), "session": sid.to_string() }),
                        );
                        for m in tmirrors {
                            self.mirror(m);
                        }
                        for f in tframes {
                            let fds: Vec<RawFd> = self.clients.keys().copied().collect();
                            for fd in fds {
                                if let Some(c) = self.clients.get_mut(&fd) {
                                    send_frame(c, &f);
                                }
                            }
                        }
                    }
                    // Phase D: workflow completion events feed triggers
                    if kind == "workflow" {
                        if let Some(st) = status.as_deref() {
                            if st == "completed" || st == "failed" {
                                let (tframes, tmirrors) = self.triggers.fire_event(
                                    "workflow_completed",
                                    &serde_json::json!({
                                        "workflow_id": pane.as_deref().unwrap_or(""),
                                        "pane": pane.as_deref().unwrap_or(""),
                                        "status": st,
                                    }),
                                );
                                for m in tmirrors {
                                    self.mirror(m);
                                }
                                for f in tframes {
                                    let fds: Vec<RawFd> = self.clients.keys().copied().collect();
                                    for fd in fds {
                                        if let Some(c) = self.clients.get_mut(&fd) {
                                            send_frame(c, &f);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // agent-tool completion hook (Phase A): a spawned
                    // pane's FIRST working->idle transition resolves its
                    // callback — AgentDone to the caller pane. The
                    // record stays (the caller can still steer/close);
                    // later idles are the agent's own business.
                    if kind == "agent" && status.as_deref() == Some("idle") {
                        let should_fire = self
                            .spawns
                            .get_by_pane(pid)
                            .is_some_and(|r| r.callback && !r.callback_fired);
                        if should_fire {
                            if let Some(r) = self.spawns.get_by_pane_mut(pid) {
                                r.callback_fired = true;
                            }
                            if let Some(rec) = self.spawns.get_by_pane(pid).cloned() {
                                let last = self
                                    .sessions
                                    .get(&sid)
                                    .and_then(|s| s.chats.get(&pid))
                                    .and_then(|cp| {
                                        cp.chat.iter().rev().find(|m| m.role == "assistant")
                                    })
                                    .cloned();
                                self.deliver_agent_done(
                                    rec.caller_pane,
                                    &rec,
                                    sid,
                                    pid,
                                    "completed",
                                    last,
                                );
                            }
                        }
                    }
                    let out = Frame::Meta {
                        session: sid.to_string(),
                        pane: Some(pid.to_string()),
                        kind: kind.clone(),
                        status: status.clone(),
                    };
                    // Agent working/idle is machine-level lifecycle info (like
                    // AgentAskRequest): every connected client gets it, so a
                    // phone sitting on the sessions list still sees turn-end
                    // for all parallel agent sessions and can notify. Model/
                    // context stay attach-scoped (per-pane UI detail).
                    let all = kind == "agent";
                    let recipients: Vec<RawFd> = self
                        .clients
                        .iter()
                        .filter(|(_, c)| all || c.attach == Some(sid))
                        .map(|(f, _)| *f)
                        .collect();
                    for rfd in recipients {
                        if let Some(c) = self.clients.get_mut(&rfd) {
                            send_frame(c, &out);
                        }
                    }
                }
                return;
            }
            Frame::Chat { session, pane, .. } if session.is_empty() => {
                let fsid = Uuid::parse_str(pane).ok();
                let found = self.sessions.iter().find_map(|(sid, s)| {
                    s.chats
                        .iter()
                        .find(|(pid, cp)| fsid == Some(**pid) || cp.forge_sid.to_string() == *pane)
                        .map(|(pid, _)| (*sid, *pid))
                });
                if let Some((sid, pid)) = found {
                    let mut filled = frame.clone();
                    if let Frame::Chat { session, pane, .. } = &mut filled {
                        *session = sid.to_string();
                        *pane = pid.to_string();
                    }
                    // apply to the local cache + broadcast
                    if let Some(s) = self.sessions.get_mut(&sid) {
                        if let Some(cp) = s.chats.get_mut(&pid) {
                            if let Frame::Chat { msgs, reset, .. } = &filled {
                                if *reset {
                                    cp.chat = msgs.clone();
                                } else {
                                    for m in msgs {
                                        if cp.chat.last().map(|l| l.seq).is_none_or(|ls| m.seq > ls)
                                        {
                                            cp.chat.push(m.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let recipients: Vec<RawFd> = self
                        .clients
                        .iter()
                        .filter(|(_, c)| c.attach == Some(sid))
                        .map(|(f, _)| *f)
                        .collect();
                    for rfd in recipients {
                        if let Some(c) = self.clients.get_mut(&rfd) {
                            send_frame(c, &filled);
                        }
                    }
                }
                return;
            }
            // client -> daemon: paged chat history (mobile/web scrollback)
            Frame::ChatHistory { session, pane, req_id, limit, before, .. } => {
                let pid = match Uuid::parse_str(pane) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let page = self
                    .resolve_session(session)
                    .and_then(|s| s.chats.get(&pid))
                    .map(|cp| {
                        let lim = (*limit).clamp(1, 200) as usize;
                        let rows: Vec<&ChatMsg> = cp
                            .chat
                            .iter()
                            .filter(|m| before.map_or(true, |b| m.seq < b))
                            .collect();
                        let start = rows.len().saturating_sub(lim);
                        (
                            rows[start..].iter().cloned().cloned().collect(),
                            start > 0,
                        )
                    });
                if let Some(c) = self.clients.get_mut(&from) {
                    match page {
                        Some((msgs, has_more)) => {
                            send_frame(
                                c,
                                &Frame::ChatHistoryOk {
                                    req_id: req_id.clone(),
                                    pane: pid.to_string(),
                                    msgs,
                                    has_more,
                                },
                            );
                        }
                        None => send_frame(
                            c,
                            &Frame::Error {
                                req_id: Some(req_id.clone()),
                                message: format!("chat pane {pane} not found"),
                            },
                        ),
                    }
                }
            }
            // pi reader / forge worker -> clients: model catalog for a
            // chat pane. Stamp the pane's model in the cache, then
            // broadcast (clients match on req_id).
            Frame::ModelListOk { pane, current, .. } => {
                if let Some(pid) = Uuid::parse_str(pane).ok() {
                    let found = self.sessions.iter().find_map(|(sid, s)| {
                        s.chats.contains_key(&pid).then_some((*sid, pid))
                    });
                    if let (Some((sid, pid)), Some(cur)) = (found, current) {
                        if let Some(s) = self.sessions.get_mut(&sid) {
                            if let Some(cp) = s.chats.get_mut(&pid) {
                                cp.model = Some(cur.name.clone());
                            }
                        }
                    }
                }
                let recipients: Vec<RawFd> = self.clients.keys().copied().collect();
                for rfd in recipients {
                    if let Some(c) = self.clients.get_mut(&rfd) {
                        send_frame(c, frame);
                    }
                }
                return;
            }
            // forge worker -> clients: a request error carrying a req_id
            // (e.g. a failed forge model switch); clients match on req_id
            Frame::Error { req_id: Some(_), .. } => {
                let recipients: Vec<RawFd> = self.clients.keys().copied().collect();
                for rfd in recipients {
                    if let Some(c) = self.clients.get_mut(&rfd) {
                        send_frame(c, frame);
                    }
                }
                return;
            }
            // forge worker -> clients: the session list reply is not
            // pane-addressable; broadcast (clients match on req_id)
            Frame::ForgeListOk { .. } => {
                let recipients: Vec<RawFd> = self.clients.keys().copied().collect();
                for rfd in recipients {
                    if let Some(c) = self.clients.get_mut(&rfd) {
                        send_frame(c, frame);
                    }
                }
                return;
            }
            // forge/mule worker -> clients: the remaining request replies
            // (profile CRUD, workflow CRUD, trigger/webhook CRUD, model
            // catalog). Not pane-addressable; broadcast (clients match on
            // req_id). Request arms live in the second match below; these
            // Ok shapes only ever arrive FROM the workers.
            Frame::ModelCatalogOk { .. }
            | Frame::ProfileListOk { .. }
            | Frame::ProfileGetOk { .. }
            | Frame::ProfilePutOk { .. }
            | Frame::ProfileDeleteOk { .. }
            | Frame::WorkflowListOk { .. }
            | Frame::MuleAgentsOk { .. }
            | Frame::WorkflowGetOk { .. }
            | Frame::WorkflowPutOk { .. }
            | Frame::WorkflowDeleteOk { .. }
            | Frame::WorkflowRunOk { .. }
            | Frame::TriggerListOk { .. }
            | Frame::TriggerPutOk { .. }
            | Frame::TriggerDeleteOk { .. }
            | Frame::WebhookListOk { .. }
            | Frame::WebhookPutOk { .. }
            | Frame::WebhookDeleteOk { .. } => {
                let recipients: Vec<RawFd> = self.clients.keys().copied().collect();
                for rfd in recipients {
                    if let Some(c) = self.clients.get_mut(&rfd) {
                        send_frame(c, frame);
                    }
                }
                return;
            }
            _ => {}
        }
        match frame {
            Frame::Hello { id, client, .. } => {
                // Log only when a client first hellos (or reconnects under
                // a new id): the dashboard re-hellos on an interval to
                // refresh the session list and would otherwise spam the log.
                let prev = self.clients.get(&from).map(|c| c.name.clone());
                if prev.as_deref() != Some(client.as_str()) {
                    eprintln!("ranchd: hello from {client} ({id})");
                }
                let sessions = self.session_meta();
                if let Some(c) = self.clients.get_mut(&from) {
                    c.name = client.clone();
                    send_frame(
                        c,
                        &Frame::HelloOk {
                            id: id.clone(),
                            machine: self.machine.clone(),
                            sessions,
                            version: Some(crate::daemon::build_version()),
                            monitor_external_pi: self.monitor_external_pi,
                        },
                    );
                }
            }
            Frame::Attach {
                id,
                client,
                session,
                chat_limit,
                ..
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(sid) => sid,
                    None => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(id.clone()),
                                    message: format!("no session {session:?}"),
                                },
                            );
                        }
                        return;
                    }
                };
                eprintln!("ranchd: {client} attached to session {sid}");
                if let Some(c) = self.clients.get_mut(&from) {
                    c.attach = Some(sid);
                    c.chat_limit = *chat_limit;
                    c.scrollback_mode = false;
                }
                self.resnap(&sid);
            }
            Frame::Detach { .. } => {
                if let Some(c) = self.clients.get_mut(&from) {
                    c.attach = None;
                }
            }
            Frame::Input {
                session,
                pane,
                data,
                ..
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let pid = match Uuid::parse_str(pane) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let bytes = match ranch_protocol::b64_decode(data) {
                    Some(b) => b,
                    None => return,
                };
                if let Some(s) = self.sessions.get_mut(&sid) {
                    if let Some(p) = s.panes.get_mut(&pid) {
                        if !p.dead {
                            let n = bytes.len();
                            let mut written = 0usize;
                            unsafe {
                                while written < n {
                                    let r = libc::write(
                                        p.master,
                                        bytes.as_ptr().add(written) as *const _,
                                        n - written,
                                    );
                                    if r <= 0 {
                                        break;
                                    }
                                    written += r as usize;
                                }
                            }
                        }
                    }
                }
            }
            Frame::Resize {
                session,
                cols,
                rows,
                ..
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let changed = {
                    let s = self.sessions.get_mut(&sid);
                    match s {
                        Some(s) if (s.size.0, s.size.1) != (*cols, *rows) => {
                            s.size = (*cols, *rows);
                            s.apply_sizes();
                            true
                        }
                        _ => false,
                    }
                };
                if changed {
                    self.resnap(&sid);
                }
            }
            Frame::ScrollbackReq {
                id,
                client: _client,
                session,
                pane,
                offset,
                limit,
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let pid = match Uuid::parse_str(pane) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let (lines, off) = {
                    let s = self.sessions.get(&sid);
                    match s.and_then(|s| s.panes.get(&pid)) {
                        Some(p) => {
                            let total = p.scrollback.len() as u64;
                            let off = if total == 0 {
                                0
                            } else {
                                (*offset).min(total - 1)
                            };
                            let end = (off + *limit as u64).min(total) as usize;
                            let start = off as usize;
                            let ls: Vec<String> = p
                                .scrollback
                                .iter()
                                .skip(start)
                                .take(end.saturating_sub(start))
                                .cloned()
                                .collect();
                            (ls, off)
                        }
                        None => (Vec::new(), 0),
                    }
                };
                if let Some(c) = self.clients.get_mut(&from) {
                    send_frame(
                        c,
                        &Frame::Scrollback {
                            id: id.clone(),
                            client: from.to_string(),
                            session: sid.to_string(),
                            pane: pid.to_string(),
                            offset: off,
                            lines,
                        },
                    );
                }
            }
            Frame::SessionsCreate {
                req_id,
                name,
                kind,
                cwd,
                profile_id,
                forge_session,
                pi_session_file,
            } => {
                let kind = kind.clone().unwrap_or_else(|| "shell".into());
                // Local-pi kill switch: `allow_local_pi = "false"` in
                // daemon.toml refuses kind=pi outright. Public demo
                // machines set this — the public must not reach any
                // code path but the sandboxed forge session tree (the
                // demo's agent is a restricted-key forge chat pane).
                if kind == "pi" && !local_pi_allowed() {
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::Error {
                                req_id: Some(req_id.clone()),
                                message: "local pi agents are disabled on this machine".into(),
                            },
                        );
                    }
                    return;
                }
                if kind != "shell" && kind != "forge" && kind != "pi" {
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::Error {
                                req_id: Some(req_id.clone()),
                                message: format!("unknown session kind {kind:?} (shell|forge|pi)"),
                            },
                        );
                    }
                    return;
                }
                let id = Uuid::new_v4();
                let name = name.clone().unwrap_or_else(|| {
                    // short, tmux-like auto name: s0, s1, ... unique in this daemon
                    let taken: std::collections::HashSet<String> =
                        self.sessions.values().map(|s| s.name.clone()).collect();
                    let mut n = 0;
                    loop {
                        let candidate = format!("s{}", n);
                        if !taken.contains(&candidate) {
                            return candidate;
                        }
                        n += 1;
                    }
                });
                let mut s = Session {
                    id,
                    name: name.clone(),
                    kind: kind.clone(),
                    panes: BTreeMap::new(),
                    active: Uuid::nil(),
                    size: (80, 24),
                    windows: vec![Window::new(
                        Layout::Leaf {
                            pane: Uuid::nil().to_string(),
                        },
                        "0".into(),
                    )],
                    win: 0,
                    chats: BTreeMap::new(),
                };
                if kind == "forge" || kind == "pi" {
                    // first-class agent session: a chat pane (no PTY).
                    // Backing resolution:
                    //   kind=forge + forge_session -> adopt (resume)
                    //   kind=forge                 -> fresh forge session
                    //   kind=pi                    -> local `pi --mode rpc`
                    let backing: Result<Uuid, String> = if kind == "pi" {
                        Ok(Uuid::nil()) // nil forge_sid = local pi backing
                    } else if let Some(fsid) = &forge_session {
                        Uuid::parse_str(fsid).map_err(|_| format!("bad forge_session id {fsid:?}"))
                    } else {
                        let forge_cfg = forge::load_forge_config();
                        forge_cfg
                            .as_ref()
                            .map(|cfg| {
                                forge::create_forge_session(
                                    cfg,
                                    &name,
                                    cwd.as_deref(),
                                    profile_id.as_deref(),
                                )
                            })
                            .unwrap_or(Err(
                                "forge not configured (set forge_api_key in daemon.toml)".into(),
                            ))
                    };
                    match backing {
                        Ok(forge_sid) => {
                            let pid = Uuid::new_v4();
                            s.chats.insert(
                                pid,
                                ChatPane {
                                    forge_sid,
                                    cols: 80,
                                    rows: 24,
                                    chat: vec![],
                                    model: None,
                                    context: None,
                                    cwd: cwd.clone(),
                                }
                            );
                            s.windows[0].layout = Layout::Leaf {
                                pane: pid.to_string(),
                            };
                            s.active = pid;
                            if kind == "pi" {
                                // local pi: spawn the rpc child now
                                let dir = cwd
                                    .clone()
                                    .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
                                match &self.forge_pipe_w {
                                    Some(pipe_w) => {
                                        if let Err(e) = pilocal::LocalPi::spawn(
                                            pid,
                                            &dir,
                                            pilocal::no_tools_configured(),
                                            pipe_w.clone(),
                                            &mut self.pi_agents,
                                        ) {
                                            eprintln!("ranchd: pi spawn failed: {e}");
                                            if let Some(c) = self.clients.get_mut(&from) {
                                                send_frame(
                                                    c,
                                                    &Frame::Error {
                                                        req_id: Some(req_id.clone()),
                                                        message: e,
                                                    },
                                                );
                                            }
                                            return;
                                        }
                                        // Resume an existing pi session file if provided.
                                        if let Some(sf) = &pi_session_file {
                                            if let Some(agent) = self.pi_agents.get(&pid) {
                                                if let Err(e) = agent.switch_session(sf) {
                                                    eprintln!("ranchd: pi switch_session failed: {e}");
                                                } else {
                                                    eprintln!("ranchd: resumed pi session from {sf}");
                                                    // show the resumed history, same as restore
                                                    // (session `s` isn't in self.sessions yet)
                                                    if let Ok(msgs) = pilocal::LocalPi::read_session_messages(sf) {
                                                        if let Some(cp) = s.chats.get_mut(&pid) {
                                                            cp.chat = msgs;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    None => {
                                        eprintln!(
                                            "ranchd: pi spawn failed: no forge pipe (forge disabled?)"
                                        );
                                    }
                                }
                                eprintln!(
                                    "ranchd: created agent session {name} ({id}) chat pane {pid} -> local pi ({dir})"
                                );
                            } else {
                                eprintln!(
                                    "ranchd: created agent session {name} ({id}) chat pane {pid} -> forge {forge_sid}"
                                );
                            }
                            // start the SSE watch on forge-backed panes
                            // (local pi panes stream from the child)
                            if kind == "forge" {
                                if let Some(tx) = &self.forge_tx {
                                    let _ = tx.send(forge::ForgeJob::Watch {
                                        pane: pid,
                                        forge_sid,
                                    });
                                }
                            }
                            let ack = Frame::SessionsAck {
                                req_id: req_id.clone(),
                                session: id.to_string(),
                                pane: pid.to_string(),
                            };
                            self.sessions.insert(id, s);
                            self.write_state();
                            self.mirror(relay::RelayOut::UpsertSession {
                                id: id.to_string(),
                                name,
                                kind,
                            });
                            if let Some(c) = self.clients.get_mut(&from) {
                                send_frame(c, &ack);
                            }
                        }
                        Err(e) => {
                            eprintln!("ranchd: agent session failed: {e}");
                            if let Some(c) = self.clients.get_mut(&from) {
                                send_frame(
                                    c,
                                    &Frame::Error {
                                        req_id: Some(req_id.clone()),
                                        message: e,
                                    },
                                );
                            }
                        }
                    }
                    return;
                }
                match spawn_pane(&mut s, &kind, cwd.as_deref()) {
                    Ok(pid) => {
                        s.windows[0].layout = Layout::Leaf {
                            pane: pid.to_string(),
                        };
                        s.active = pid;
                        eprintln!("ranchd: created session {name} ({id}) pane {pid}");
                        let ack = Frame::SessionsAck {
                            req_id: req_id.clone(),
                            session: id.to_string(),
                            pane: pid.to_string(),
                        };
                        self.sessions.insert(id, s);
                        self.write_state();
                        self.mirror(relay::RelayOut::UpsertSession {
                            id: id.to_string(),
                            name,
                            kind,
                        });
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(c, &ack);
                        }
                    }
                    Err(e) => {
                        if let Some(c) = self.clients.get_mut(&from) {
                            send_frame(
                                c,
                                &Frame::Error {
                                    req_id: Some(req_id.clone()),
                                    message: e,
                                },
                            );
                        }
                    }
                }
            }
            Frame::SessionsRename { session, name } => {
                if let Some(sid) = self.resolve_session(session).map(|s| s.id) {
                    let kind = self.sessions.get(&sid).map(|s| s.kind.clone());
                    if let Some(s2) = self.sessions.get_mut(&sid) {
                        s2.name = name.clone();
                    }
                    self.write_state();
                    if let Some(kind) = kind {
                        self.mirror(relay::RelayOut::UpsertSession {
                            id: sid.to_string(),
                            name: name.clone(),
                            kind,
                        });
                    }
                }
            }
            Frame::SessionsKill { session } => {
                if let Some(sid) = self.resolve_session(session).map(|s| s.id) {
                    if let Some(s) = self.sessions.remove(&sid) {
                        for p in s.panes.values() {
                            self.orphans.push(p.child);
                        }
                        // stop the chat panes' backing agents
                        for pid in s.chats.keys() {
                            if let Some(lp) = self.pi_agents.remove(pid) {
                                if let Some(pid) = lp.kill() {
                                    self.orphans.push(pid);
                                }
                            }
                            if let Some(tx) = &self.forge_tx {
                                let _ = tx.send(forge::ForgeJob::Unwatch { pane: *pid });
                            }
                        }
                        eprintln!("ranchd: killed session {}", s.name);
                    }
                    self.write_state();
                    self.mirror(relay::RelayOut::DeleteSession {
                        id: sid.to_string(),
                    });
                    let gone = Frame::Meta {
                        session: sid.to_string(),
                        pane: None,
                        kind: "exited".into(),
                        status: Some("session killed".into()),
                    };
                    let recipients: Vec<RawFd> = self.clients.iter().map(|(f, _)| *f).collect();
                    for rfd in recipients {
                        if let Some(c) = self.clients.get_mut(&rfd) {
                            if c.attach == Some(sid) {
                                c.attach = None;
                            }
                            send_frame(c, &gone);
                        }
                    }
                }
            }
            Frame::SessionsSelect { session, pane } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let pid = match Uuid::parse_str(pane) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let known = {
                    let s = self.sessions.get_mut(&sid);
                    s.is_some_and(|s| {
                        if s.panes.contains_key(&pid) {
                            s.active = pid;
                            true
                        } else {
                            false
                        }
                    })
                };
                if known {
                    self.resnap(&sid);
                }
            }
            Frame::PaneSplit {
                req_id,
                session,
                pane,
                dir,
                kind,
            } => {
                // Real split: replace the target leaf with a split node,
                // spawn the new pane, then re-apply sizes (sibling shrinks).
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let dir = *dir.min(&1); // 0 = top/bottom, 1 = left/right
                let kind = kind.clone().unwrap_or_else(|| "shell".into());
                let new_pane = self.sessions.get_mut(&sid).and_then(|s| {
                    let target = match Uuid::parse_str(pane) {
                        Ok(p) if s.panes.contains_key(&p) => p,
                        _ => s.active,
                    };
                    // agent split: chat pane bound to a fresh forge session
                    if kind == "forge" || kind == "pi" {
                        let fcfg = forge::load_forge_config();
                        // anchor the agent to the focused pane's cwd so
                        // terminal and agent work the same tree
                        let anchor_dir = (|| {
                            let fpid = s.panes.get(&target)?;
                            pane_cwd(fpid.child)
                        })();
                        // pi split: local `pi --mode rpc` child, no forge
                        if kind == "pi" {
                            let pid = Uuid::new_v4();
                            let dir_str = anchor_dir
                                .clone()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
                            match &self.forge_pipe_w {
                                Some(pipe_w) => {
                                    if let Err(e) = pilocal::LocalPi::spawn(
                                        pid,
                                        &dir_str,
                                        pilocal::no_tools_configured(),
                                        pipe_w.clone(),
                                        &mut self.pi_agents,
                                    ) {
                                        eprintln!("ranchd: pi split failed: {e}");
                                        if let Some(c) = self.clients.get_mut(&from) {
                                            send_frame(
                                                c,
                                                &Frame::Error {
                                                    req_id: Some(req_id.clone()),
                                                    message: e,
                                                },
                                            );
                                        }
                                        return None;
                                    }
                                }
                                None => {
                                    eprintln!("ranchd: pi split failed: no forge pipe");
                                    return None;
                                }
                            }
                            s.chats.insert(
                                pid,
                                ChatPane {
                                    forge_sid: Uuid::nil(),
                                    cols: 80,
                                    rows: 24,
                                    chat: vec![],
                                    model: None,
                                    context: None,
                                    cwd: None,
                                }
                            );
                            s.win_mut()
                                .split_leaf(&target.to_string(), &pid.to_string(), dir);
                            s.active = pid;
                            s.apply_sizes();
                            eprintln!(
                                "ranchd: agent split: chat pane {pid} -> local pi ({dir_str})"
                            );
                            return Some(pid);
                        }
                        let forge_sid = match fcfg
                            .as_ref()
                            .map(|c| {
                                forge::create_forge_session(
                                    c,
                                    "agent pane",
                                    anchor_dir
                                        .as_deref()
                                        .map(|p| p.to_string_lossy())
                                        .as_deref(),
                                    None,
                                )
                            })
                            .unwrap_or(Err("forge not configured".into()))
                        {
                            Ok(id) => id,
                            Err(e) => {
                                eprintln!("ranchd: agent split failed: {e}");
                                if let Some(c) = self.clients.get_mut(&from) {
                                    send_frame(
                                        c,
                                        &Frame::Error {
                                            req_id: Some(req_id.clone()),
                                            message: e,
                                        },
                                    );
                                }
                                return None;
                            }
                        };
                        let pid = Uuid::new_v4();
                        s.chats.insert(
                            pid,
                            ChatPane {
                                forge_sid,
                                cols: 80,
                                rows: 24,
                                chat: vec![],
                                model: None,
                                context: None,
                                cwd: None,
                            }
                        );
                        s.win_mut()
                            .split_leaf(&target.to_string(), &pid.to_string(), dir);
                        s.active = pid;
                        s.apply_sizes();
                        if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::Watch {
                                pane: pid,
                                forge_sid,
                            });
                        }
                        eprintln!("ranchd: agent split: chat pane {pid} -> forge {forge_sid}");
                        return Some(pid);
                    }
                    match spawn_pane(s, "shell", None) {
                        Ok(pid) => {
                            s.win_mut()
                                .split_leaf(&target.to_string(), &pid.to_string(), dir);
                            s.active = pid;
                            s.apply_sizes();
                            Some(pid)
                        }
                        Err(e) => {
                            eprintln!("ranchd: split failed: {e}");
                            None
                        }
                    }
                });
                if let Some(pid) = new_pane {
                    self.write_state();
                    self.resnap(&sid);
                    if let Some(c) = self.clients.get_mut(&from) {
                        send_frame(
                            c,
                            &Frame::SessionsAck {
                                req_id: req_id.clone(),
                                session: sid.to_string(),
                                pane: pid.to_string(),
                            },
                        );
                    }
                }
            }
            Frame::PaneResize {
                session,
                pane,
                dir,
                delta,
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let pid = match Uuid::parse_str(pane) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let did = self.sessions.get_mut(&sid).map(|s| {
                    s.resize_split(pid, *dir, *delta);
                    s.apply_sizes();
                });
                if did.is_some() {
                    self.resnap(&sid);
                }
            }
            Frame::PaneSwap { session, a, b } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let did = self.sessions.get_mut(&sid).map(|s| {
                    let ok = s.win_mut().swap_leaves(a, b);
                    if ok {
                        // focus stays on the same pane (tmux swap-panes)
                    }
                    if ok {
                        s.apply_sizes();
                    }
                    ok
                });
                if did.unwrap_or(false) {
                    self.resnap(&sid);
                }
            }
            Frame::WindowNew {
                req_id,
                session,
                name,
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let new_win = self.sessions.get_mut(&sid).and_then(|s| {
                    match spawn_pane(s, "shell", None) {
                        Ok(pid) => {
                            // auto name: window number, unique in the session
                            let wname = name.clone().unwrap_or_else(|| {
                                let mut n = 0;
                                loop {
                                    let cand = n.to_string();
                                    if s.windows.iter().all(|w| w.name != cand) {
                                        return cand;
                                    }
                                    n += 1;
                                }
                            });
                            let w = Window::new(
                                Layout::Leaf {
                                    pane: pid.to_string(),
                                },
                                wname,
                            );
                            let wid = w.id;
                            s.windows.push(w);
                            s.win = s.windows.len() - 1;
                            s.active = pid;
                            s.apply_sizes();
                            eprintln!("ranchd: window created in {sid} (pane {pid})");
                            if let Some(c) = self.clients.get_mut(&from) {
                                send_frame(
                                    c,
                                    &Frame::SessionsAck {
                                        req_id: req_id.clone(),
                                        session: sid.to_string(),
                                        pane: pid.to_string(),
                                    },
                                );
                            }
                            Some(wid)
                        }
                        Err(e) => {
                            eprintln!("ranchd: window create failed: {e}");
                            None
                        }
                    }
                });
                if new_win.is_some() {
                    self.write_state();
                    self.resnap(&sid);
                }
            }
            Frame::WindowSelect { session, window } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let did = self.sessions.get_mut(&sid).map(|s| {
                    let idx = s.windows.iter().position(|w| w.id.to_string() == *window);
                    if let Some(i) = idx {
                        s.win = i;
                        s.active = s.windows[i]
                            .pane_ids()
                            .first()
                            .copied()
                            .unwrap_or_else(Uuid::nil);
                        s.apply_sizes();
                    }
                    idx.is_some()
                });
                if did.unwrap_or(false) {
                    self.write_state();
                    self.resnap(&sid);
                }
            }
            Frame::WindowNext { session, delta } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let n = self.sessions.get_mut(&sid).map(|s| {
                    let len = s.windows.len() as i32;
                    let cur = s.win as i32;
                    let next = ((cur + *delta as i32) % len + len) % len;
                    s.win = next as usize;
                    s.active = s.windows[next as usize]
                        .pane_ids()
                        .first()
                        .copied()
                        .unwrap_or_else(Uuid::nil);
                    s.apply_sizes();
                    s.windows[next as usize].id.to_string()
                });
                if n.is_some() {
                    self.write_state();
                    self.resnap(&sid);
                }
            }
            Frame::WindowKill { session, window } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let outcome = self.sessions.get_mut(&sid).and_then(|s| {
                    let idx = s.windows.iter().position(|w| w.id.to_string() == *window)?;
                    let w = s.windows.remove(idx);
                    for pid in w.pane_ids() {
                        if let Some(p) = s.panes.remove(&pid) {
                            self.orphans.push(p.child);
                        }
                    }
                    if s.win >= s.windows.len() {
                        s.win = s.windows.len().saturating_sub(1);
                    }
                    if !s.windows.is_empty() {
                        s.active = s
                            .win()
                            .pane_ids()
                            .first()
                            .copied()
                            .unwrap_or_else(Uuid::nil);
                    }
                    Some(s.windows.is_empty())
                });
                match outcome {
                    Some(true) | None => {
                        // last window gone: kill the whole session
                        if let Some(s) = self.sessions.remove(&sid) {
                            for p in s.panes.values() {
                                self.orphans.push(p.child);
                            }
                            eprintln!("ranchd: killed session {} (last window closed)", s.name);
                        }
                        self.write_state();
                        self.mirror(relay::RelayOut::DeleteSession {
                            id: sid.to_string(),
                        });
                    }
                    Some(false) => {
                        self.write_state();
                        self.resnap(&sid);
                    }
                }
            }
            Frame::WindowRename {
                session,
                window,
                name,
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let did = self.sessions.get_mut(&sid).map(|s| {
                    match s.windows.iter_mut().find(|w| w.id.to_string() == *window) {
                        Some(w) => {
                            w.name = name.clone();
                            true
                        }
                        None => false,
                    }
                });
                if did.unwrap_or(false) {
                    self.write_state();
                    self.resnap(&sid);
                }
            }
            Frame::PaneKill { session, pane } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let pid = match Uuid::parse_str(pane) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let mut need_snap = None;
                if let Some(s) = self.sessions.get_mut(&sid) {
                    if let Some(p) = s.panes.remove(&pid) {
                        // master fd drops here (SIGHUP to the shell); reap later
                        self.orphans.push(p.child);
                    }
                    // chat pane kill: stop its backing agent
                    if s.chats.remove(&pid).is_some() {
                        if let Some(lp) = self.pi_agents.remove(&pid) {
                            if let Some(pid) = lp.kill() {
                                self.orphans.push(pid);
                            }
                        }
                        if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::Unwatch { pane: pid });
                        }
                    }
                    let dead = s.remove_pane_everywhere(&pid.to_string());
                    s.apply_sizes();
                    // the session dies when NOTHING is left — count chat
                    // panes too. `s.panes` is PTY panes only, so an agent
                    // session (all chat panes) used to be torn down on the
                    // first Ctrl-B x even with another chat pane still open.
                    if dead || (s.panes.is_empty() && s.chats.is_empty()) {
                        need_snap = None;
                        eprintln!("ranchd: last pane closed, killing session {}", s.name);
                        self.sessions.remove(&sid);
                        // tell everyone, same as a pane exit / :kill —
                        // otherwise attached TUIs sit on a dead session
                        self.mirror(relay::RelayOut::DeleteSession {
                            id: sid.to_string(),
                        });
                        let gone = Frame::Meta {
                            session: sid.to_string(),
                            pane: None,
                            kind: "exited".into(),
                            status: Some("session ended".into()),
                        };
                        let recipients: Vec<RawFd> =
                            self.clients.iter().map(|(f, _)| *f).collect();
                        for rfd in recipients {
                            if let Some(c) = self.clients.get_mut(&rfd) {
                                if c.attach == Some(sid) {
                                    c.attach = None;
                                }
                                send_frame(c, &gone);
                            }
                        }
                    } else {
                        need_snap = Some(sid);
                    }
                }
                self.write_state();
                if let Some(sid) = need_snap {
                    self.resnap(&sid);
                }
            }
            Frame::Hb
            | Frame::Meta { .. }
            | Frame::Error { .. }
            | Frame::Chunk { .. }
            | Frame::HelloOk { .. }
            | Frame::Snapshot { .. }
            | Frame::Update { .. }
            | Frame::SessionsAck { .. }
            | Frame::Scrollback { .. } => {
                // client->daemon direction: not expected; ignore
            }
            _ => {}
        }
    }
}

// ---------- main loop ----------

static RUNNING: AtomicUsize = AtomicUsize::new(1);
extern "C" fn on_signal(_sig: c_int) {
    // SAFETY: a lock-free word store is async-signal-safe on x86-64.
    RUNNING.store(0, Ordering::SeqCst);
}

pub(super) use crate::{forge, pilocal, relay};

pub fn run_daemon() {
    let args: Vec<String> = std::env::args().collect();
    run_daemon_with_args(args);
}

pub fn run_daemon_with_args(args: Vec<String>) {
    // hot-upgrade inherit path: the previous generation exec'd us with a
    // manifest; adopt its sessions/panes/pi-children and run the same loop
    // (when invoked as `ranch daemon`, the subcommand sits at args[1] and
    // the real flags start at args[2] — normalize so --inherit is args[1])
    let args: Vec<String> = if args.len() >= 2 && args[1] == "daemon" {
        [args[0].clone()].into_iter().chain(args.into_iter().skip(2)).collect()
    } else {
        args
    };
    if args.len() >= 3 && args[1] == "--inherit" {
        // --listen-fd=N: reuse the previous generation's listening socket
        // (bind would fail with EADDRINUSE — the inherited fd still holds it)
        let listen_fd = args
            .iter()
            .find_map(|a| a.strip_prefix("--listen-fd=").map(|v| v.to_string()));
        let daemon = match Daemon::inherit(&args[2], listen_fd) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("ranchd: inherit failed: {e} — falling back to cold start");
                match Daemon::new() {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("ranchd: {e}");
                        std::process::exit(1);
                    }
                }
            }
        };
        run(daemon, spawn_control_api());
        return;
    }
    if args.len() >= 2 && args[1] == "upgrade" {
        eprintln!("ranchd: `upgrade` is sent as a frame to the running daemon (ranch upgrade)");
        std::process::exit(2);
    }
    let control_rx = spawn_control_api();
    let daemon = match Daemon::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ranchd: {e}");
            std::process::exit(1);
        }
    };
    run(daemon, control_rx);
}

/// Loopback control API for agent tools (Phase A). MUST run before any
/// pi pane spawn (cold start's restore_state() respawns saved panes and
/// they inherit RANCH_CONTROL_* env at spawn — a control API started
/// after restore leaves every restored pane without agent tools until
/// the next manual spawn). The accept thread blocks on the reply
/// channel, so an agent's tool call is synchronous end to end.
fn spawn_control_api() -> Option<std::sync::mpsc::Receiver<control_api::ControlRequest>> {
    match control_api::spawn() {
        Ok((port, rx)) => {
            eprintln!("ranchd: control api on 127.0.0.1:{port} (agent tools)");
            Some(rx)
        }
        Err(e) => {
            eprintln!("ranchd: control api disabled: {e}");
            None
        }
    }
}

fn run(
    mut daemon: Daemon,
    control_rx: Option<std::sync::mpsc::Receiver<control_api::ControlRequest>>,
) {
    // editor file-watch poll cadence (M10 ph3): stat the watched files
    // every ~2 s (66 x 30 ms ticks)
    let mut fw_tick: u32 = 0;
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as usize;
        sa.sa_flags = 0;
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        sa.sa_mask = set;
        libc::sigaction(SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }

    loop {
        // --- build poll set ---
        let mut fds: Vec<pollfd> = Vec::new();
        fds.push(pollfd {
            fd: daemon.listener.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        });
        for c in daemon.clients.values() {
            match (&c.stream, &c.relay_in) {
                (Some(s), _) => fds.push(pollfd {
                    fd: s.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                }),
                (None, Some(w)) => fds.push(pollfd {
                    fd: w.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                }),
                (None, None) => {}
            }
        }
        for s in daemon.sessions.values() {
            for p in s.panes.values() {
                fds.push(pollfd {
                    fd: p.master,
                    events: POLLIN,
                    revents: 0,
                });
            }
        }

        let timeout: c_int = if RUNNING.load(Ordering::SeqCst) != 0 {
            TICK_MS
        } else {
            -1
        };
        // local-pi session files get captured asynchronously (pi's
        // get_state response); re-persist state.json when a capture lands
        if pilocal::STATE_DIRTY.swap(false, Ordering::Relaxed) {
            daemon.write_state();
        }
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as u64, timeout) };
        if n < 0 {
            eprintln!("ranchd: poll: {:?}", std::io::Error::last_os_error());
            if RUNNING.load(Ordering::SeqCst) == 0 {
                break;
            }
            continue;
        }

        // --- accept new clients ---
        if fds[0].revents & POLLIN != 0 {
            if let Ok((stream, _addr)) = daemon.listener.accept() {
                let fd = stream.as_raw_fd();
                eprintln!("ranchd: client {fd} connected");
                daemon.clients.insert(
                    fd,
                    Client {
                        stream: Some(stream),
                        relay_out: None,
                        relay_in: None,
                        sink: None,
                        decoder: Decoder::new(),
                        name: format!("cli-{fd}"),
                        attach: None,
                        chat_limit: None,
                        scrollback_mode: false,
                        file_watches: BTreeMap::new(),
                    },
                );
            }
        }

        // --- client I/O + frame collection ---
        let mut gone: Vec<RawFd> = Vec::new();
        let mut frame_queue: Vec<(RawFd, Frame)> = Vec::new();
        for (fd, c) in daemon.clients.iter_mut() {
            let is_relevant = fds
                .iter()
                .position(|f| f.fd == *fd)
                .is_some_and(|i| fds[i].revents & (POLLIN | POLLHUP | POLLERR) != 0);
            if !is_relevant {
                continue;
            }
            let mut buf = [0u8; 65536];
            let r = if let Some(stream) = &mut c.stream {
                match stream.read(&mut buf) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("ranchd: client {fd} read: {e}");
                        gone.push(*fd);
                        continue;
                    }
                }
            } else if let Some(pin) = &mut c.relay_in {
                use std::os::fd::AsRawFd;
                let r =
                    unsafe { libc::read(pin.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
                if r < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        eprintln!("ranchd: relay pipe read: {err}");
                    }
                    continue;
                }
                r as usize
            } else {
                continue;
            };
            if r == 0 {
                // relay client: pipe closing means the thread exited; keep
                // the client (the thread reconnects and keeps its ends)
                if c.relay_in.is_some() {
                    continue;
                }
                gone.push(*fd);
                continue;
            }
            for frame in c.decoder.feed(&buf[..r]) {
                frame_queue.push((*fd, frame));
            }
        }
        for fd in gone {
            if let Some(c) = daemon.clients.remove(&fd) {
                eprintln!("ranchd: client {} ({}) disconnected", fd, c.name);
            }
        }

        // --- process frames ---
        for (fd, frame) in frame_queue {
            daemon.handle_frame(fd, &frame);
        }

        // --- control API: agent tool requests (Phase A). Each request
        // is handled as a transient pseudo-client whose "stream" is a
        // sink; the collected reply frames go back to the agent's HTTP
        // response. The pseudo-client's caller pane makes the ownership
        // checks in handle_agent_* work. ---
        if let Some(rx) = &control_rx {
            loop {
                match rx.try_recv() {
                    Ok(req) => {
                        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                        // transient fd slot that cannot collide with real
                        // clients (poll fds are >= 3, sockets positive)
                        const CTL_FD: RawFd = -1000;
                        daemon.clients.insert(
                            CTL_FD,
                            Client {
                                stream: None,
                                relay_out: None,
                                relay_in: None,
                                sink: Some(sink.clone()),
                                decoder: Decoder::new(),
                                name: format!("ctl-{}", req.caller_pane),
                                attach: None,
                                chat_limit: None,
                                scrollback_mode: false,
                                file_watches: BTreeMap::new(),
                            },
                        );
                        daemon.handle_frame(CTL_FD, &req.frame);
                        daemon.clients.remove(&CTL_FD);
                        let frames = sink.lock().map(|g| g.clone()).unwrap_or_default();
                        let _ = req.reply.send(frames);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        // --- file watches: external edits to open editor files (M10 ph3).
        // Cheap metadata stats on a handful of files; a changed mtime
        // pushes FileChanged to the watching client (baseline refreshes
        // on FileRead/FileWriteOk so our own saves stay quiet). ---
        // trigger scheduler: cron evaluation once per second (the
        // scheduler dedupes to once per trigger per minute)
        {
            let (frames, mirrors) = daemon.triggers.tick();
            for m in mirrors {
                daemon.mirror(m);
            }
            if !frames.is_empty() {
                let fds: Vec<RawFd> = daemon.clients.keys().copied().collect();
                for f in frames {
                    for fd in &fds {
                        if let Some(c) = daemon.clients.get_mut(fd) {
                            send_frame(c, &f);
                        }
                    }
                }
            }
        }
        // agent-tool `ask` policy: expire unapproved spawn requests
        for (spawn_id, caller) in daemon.spawns.expire_pending() {
            eprintln!("ranchd: spawn request {spawn_id} expired (no approval)");
            let rec = agenttools::SpawnRecord {
                spawn_id: spawn_id.clone(),
                caller_pane: caller,
                created_at: Instant::now(),
                callback: true,
                callback_fired: false,
            };
            daemon.deliver_agent_done(caller, &rec, uuid::Uuid::nil(), uuid::Uuid::nil(), "denied", None);
        }
        // agent ask-user questions: prune resolved/expired entries
        for id in daemon.asks.prune() {
            eprintln!("ranchd: ask {id} pruned from registry");
        }
        fw_tick = fw_tick.wrapping_add(1);
        if fw_tick.wrapping_rem(66) == 0 {
            for c in daemon.clients.values_mut() {
                if c.file_watches.is_empty() {
                    continue;
                }
                let mut notes: Vec<(String, i64)> = Vec::new();
                let mut drop: Vec<String> = Vec::new();
                for (path, last) in c.file_watches.iter_mut() {
                    match std::fs::metadata(path) {
                        Ok(m) => {
                            let cur = m
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            if cur != *last {
                                *last = cur;
                                notes.push((path.clone(), cur));
                            }
                        }
                        Err(_) => drop.push(path.clone()), // file gone
                    }
                }
                for p in drop {
                    c.file_watches.remove(&p);
                }
                for (path, mtime) in notes {
                    send_frame(
                        c,
                        &Frame::FileChanged { path, mtime },
                    );
                }
            }
        }

        // --- pty reads ---
        for s in daemon.sessions.values_mut() {
            for p in s.panes.values_mut() {
                if p.dead {
                    continue;
                }
                let idx = fds.iter().position(|f| f.fd == p.master);
                if let Some(i) = idx {
                    if fds[i].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                        let mut buf = [0u8; 65536];
                        loop {
                            let r = unsafe {
                                libc::read(p.master, buf.as_mut_ptr() as *mut _, buf.len())
                            };
                            if r <= 0 {
                                break;
                            }
                            p.vt.write(&buf[..r as usize]);
                            p.dirty = true;
                            if r < buf.len() as isize {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // --- child liveness + tick: coalesce dirty panes into updates ---
        let mut updates: Vec<Frame> = Vec::new();
        let mut died: Vec<(Uuid, Uuid)> = Vec::new();
        for (sid, s) in daemon.sessions.iter_mut() {
            let alive = s.panes.values().any(|pp| !pp.dead);
            for (pid, p) in s.panes.iter_mut() {
                if !p.dead {
                    let mut status: c_int = 0;
                    let w = unsafe { waitpid(p.child, &mut status, WNOHANG) };
                    if w == p.child {
                        p.dead = true;
                        let detail = if status & 0x130 == 0x130 {
                            format!("exit {}", (status >> 8) & 0xff)
                        } else {
                            format!("signal {}", (status & 0x7f) + 1)
                        };
                        eprintln!("ranchd: pane {pid} ({}:{}) child exited ({})", s.name, s.id, detail);
                        died.push((*sid, *pid));
                    }
                }
                if p.dirty && alive {
                    let (pcols, prows) = p.vt.dims();
                    let new_screen = p.vt.screen();
                    let (cx, cy, vis) = p.vt.cursor();
                    // Scrollback capture: the formatter's full area is
                    // [scrollback rows..., visible rows...]. The scrollbar
                    // total/len tell us where the visible viewport starts,
                    // so the ring is simply the rows above it — rebuilt
                    // wholesale each tick (cheap: <= cap+rows lines).
                    let full = p.vt.full_screen();
                    let (total, _, vlen) = p.vt.scrollbar();
                    let visible_start = total.saturating_sub(vlen) as usize;
                    if full.len() >= visible_start {
                        p.scrollback = full[..visible_start]
                            .iter()
                            .filter(|l| !l.is_empty())
                            .cloned()
                            .collect::<VecDeque<String>>();
                        if p.scrollback.len() > SCROLLBACK_CAP {
                            let drop = p.scrollback.len() - SCROLLBACK_CAP;
                            p.scrollback.drain(..drop);
                        }
                    }

                    let mut rows_upd: Vec<(u16, String)> = Vec::new();
                    for (i, line) in new_screen.iter().enumerate() {
                        let old = p.prev_screen.get(i).map(|s| s.as_str());
                        if old != Some(line.as_str()) {
                            rows_upd.push((i as u16, line.clone()));
                        }
                    }
                    p.prev_screen = new_screen;
                    p.dirty = false;
                    p.seq += 1;
                    updates.push(Frame::Update {
                        id: Uuid::new_v4().to_string(),
                        client: String::new(),
                        session: s.id.to_string(),
                        pane: pid.to_string(),
                        seq: p.seq,
                        cols: pcols,
                        rows: prows,
                        rows_upd,
                        cursor: Some(Cursor {
                            x: cx,
                            y: cy,
                            visible: vis,
                        }),
                        title: None,
                    });
                }
            }
        }
        // deliver updates to attached clients
        for frame in updates {
            let sid = match &frame {
                Frame::Update { session, .. } => Uuid::parse_str(session).ok(),
                _ => None,
            };
            if let Some(sid) = sid {
                let recipients: Vec<RawFd> = daemon
                    .clients
                    .iter()
                    .filter(|(_, c)| c.attach == Some(sid))
                    .map(|(f, _)| *f)
                    .collect();
                for rfd in recipients {
                    if let Some(c) = daemon.clients.get_mut(&rfd) {
                        send_frame(c, &frame);
                    }
                }
            }
        }

        // exited children close their pane (tmux semantics: shell exit
        // = pane gone; last pane = session gone)
        for (sid, pid) in died {
            let Some(s) = daemon.sessions.get_mut(&sid) else {
                continue;
            };
            if let Some(p) = s.panes.remove(&pid) {
                daemon.orphans.push(p.child);
            }
            let gone_last = s.remove_pane_everywhere(&pid.to_string());
            s.apply_sizes();
            // chat panes count as alive too (a shell exiting next to an
            // agent pane must not kill the agent)
            if gone_last || (s.panes.is_empty() && s.chats.is_empty()) {
                eprintln!("ranchd: last pane exited, killing session {}", s.name);
                daemon.sessions.remove(&sid);
                daemon.write_state();
                daemon.mirror(relay::RelayOut::DeleteSession {
                    id: sid.to_string(),
                });
                let gone = Frame::Meta {
                    session: sid.to_string(),
                    pane: None,
                    kind: "exited".into(),
                    status: Some("session ended".into()),
                };
                let recipients: Vec<RawFd> = daemon.clients.iter().map(|(f, _)| *f).collect();
                for rfd in recipients {
                    if let Some(c) = daemon.clients.get_mut(&rfd) {
                        if c.attach == Some(sid) {
                            c.attach = None;
                        }
                        send_frame(c, &gone);
                    }
                }
            } else {
                daemon.write_state();
                daemon.resnap(&sid);
            }
        }

        // --- reap orphaned pane children (killed panes/sessions) ---
        if !daemon.orphans.is_empty() {
            let mut status: c_int = 0;
            daemon.orphans.retain(|&pid| {
                let w = unsafe { waitpid(pid, &mut status, WNOHANG) };
                if w == pid {
                    eprintln!("ranchd: reaped orphan child {pid}");
                    return false; // reaped — drop
                }
                // ECHILD => already gone; WNOHANG 0 => still running; keep both
                !(w == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
            });
        }

        // --- shutdown ---
        if RUNNING.load(Ordering::SeqCst) == 0 {
            break;
        }
    }

    eprintln!("ranchd: shutting down");
    // Only unlink if nobody is listening on the path. Our own listener is
    // still open here (dropped at end of scope), so this skips removal in
    // the normal case — leaving our stale file, which the next cold start
    // probes (ECONNREFUSED) and reclaims. It also protects the
    // stop→start race: if a newer generation already bound the path, we
    // must not unlink its live socket.
    if !socket_has_live_listener(&daemon.socket_path) {
        let _ = std::fs::remove_file(&daemon.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_session_meta_extracts_cwd_and_last_user_message() {
        let dir = std::env::temp_dir().join(format!("ranch-pi-meta-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess.jsonl");
        let content = [
            r#"{"type":"session","id":"abc","cwd":"/home/u/src/lab"}"#,
            r#"{"type":"model_change","id":"m1"}"#,
            r#"{"type":"message","id":"1","message":{"role":"user","content":[{"type":"text","text":"fix the build please"}],"timestamp":1}}"#,
            r#"{"type":"message","id":"2","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]},"timestamp":2}}"#,
            r#"{"type":"message","id":"3","message":{"role":"user","content":[{"type":"text","text":"and more"}],"timestamp":3}}"#,
        ]
        .join("\n");
        std::fs::write(&path, content).unwrap();

        let (sid, title, cwd) = pi_session_meta(&path.to_string_lossy());
        assert_eq!(sid.as_deref(), Some("abc"));
        assert_eq!(cwd.as_deref(), Some("/home/u/src/lab"));
        assert_eq!(title.as_deref(), Some("and more"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pi_session_meta_missing_file_is_none() {
        let (sid, title, cwd) = pi_session_meta("/nonexistent/path.jsonl");
        assert_eq!(sid, None);
        assert_eq!(title, None);
        assert_eq!(cwd, None);
    }

    #[test]
    fn socket_has_live_listener_detects_live_and_stale_sockets() {
        let dir = std::env::temp_dir().join(format!("ranch-sock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.sock");
        let _ = std::fs::remove_file(&path);

        // Live listener: connect succeeds → occupied.
        let listener = UnixListener::bind(&path).unwrap();
        assert!(socket_has_live_listener(&path));

        // Drop the listener without unlinking: the file remains but is
        // dead → connect refused → safe to reclaim.
        drop(listener);
        assert!(!socket_has_live_listener(&path));

        let _ = std::fs::remove_file(&path);
        // Missing file → not occupied.
        assert!(!socket_has_live_listener(&path));
        let _ = std::fs::remove_dir(&dir);
    }
}
