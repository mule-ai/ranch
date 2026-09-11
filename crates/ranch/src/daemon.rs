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
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use libc::{SIGINT, WNOHANG, c_int, pollfd};
use ranch_protocol::{ChatMsg, Cursor, Decoder, Frame, Layout, PaneSnap, SessionMeta};
use ranch_vt::Vt;
use uuid::Uuid;

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
    decoder: Decoder,
    name: String,
    /// Session currently attached; None = not attached.
    attach: Option<Uuid>,
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

fn home_dir_string() -> String {
    home_dir().to_string_lossy().into_owned()
}

/// The Tier-1 persistence document (state.json contents / manifest state
/// section): sessions with panes, chats, windows, layouts.
fn state_value(
    sessions: &BTreeMap<Uuid, Session>,
    pi_agents: &BTreeMap<Uuid, std::sync::Arc<pilocal::LocalPi>>,
) -> serde_json::Value {
    serde_json::json!({
        "machine": hostname(),
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
    })
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
            // shim PATH from the user profile applies); shells run bare
            let shell = default_shell();
            let shell_c = shell.as_ptr();
            if pane_kind == "forge" {
                let dir = cwd.map(std::path::PathBuf::from).unwrap_or_else(home_dir);
                let d_c =
                    std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap_or_default();
                if unsafe { libc::chdir(d_c.as_ptr()) } != 0 {
                    eprintln!("ranchd: chdir {:?} failed", dir);
                }
                let arg = b"exec pi\0";
                let argv: [*const u8; 4] =
                    [shell_c, b"-lc\0".as_ptr(), arg.as_ptr(), std::ptr::null()];
                execvp(shell_c, argv.as_ptr());
            } else {
                let argv: [*const u8; 2] = [shell_c, std::ptr::null()];
                execvp(shell_c, argv.as_ptr());
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
fn snapshot_session(s: &Session) -> Option<Frame> {
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
        };
        if *pid == s.active {
            active_seq = p.seq;
        }
        snaps.push(snap);
    }
    for (pid, cp) in &s.chats {
        let (cols, rows) = sizes.get(pid).copied().unwrap_or((cp.cols, cp.rows));
        snaps.push(PaneSnap {
            id: pid.to_string(),
            cols,
            rows,
            lines: vec![],
            seq: 0,
            cursor: None,
            kind: Some("forge-chat".into()),
            chat: Some(cp.chat.clone()),
            forge_session: Some(cp.forge_sid.to_string()),
            model: cp.model.clone(),
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
            (None, None) => Ok(()),
        };
        if let Err(e) = res {
            eprintln!("ranchd: write: {e}");
            break;
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
                        decoder: Decoder::new(),
                        name: "relay".into(),
                        attach: None,
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

        // forge worker: chat-pane polling + sends on a dedicated thread
        // (blocking HTTP must never stall the poll loop); results come
        // back as frame lines on a pipe, read like any client below
        let forge_tx = if let Some(fcfg) = forge::load_forge_config() {
            // the forge worker gets its OWN pipe pair: worker frames ->
            // daemon (daemon_r/daemon_w); the second pair is ignored
            // (make_pipes always returns two)
            match relay::make_pipes() {
                Ok(((f_daemon_r, f_daemon_w), (_u1, _u2))) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    if let Ok(clone) = f_daemon_w.try_clone() {
                        daemon.forge_pipe_w =
                            Some(std::sync::Arc::new(std::sync::Mutex::new(clone)));
                    }
                    forge::spawn_worker(fcfg, f_daemon_w, rx);
                    let fclient = Client {
                        stream: None,
                        relay_out: None,
                        relay_in: Some(fd_file(f_daemon_r)),
                        decoder: Decoder::new(),
                        name: "forge".into(),
                        attach: None,
                        scrollback_mode: false,
                        file_watches: BTreeMap::new(),
                    };
                    if let Some(rfd) = fclient.relay_in.as_ref().map(|f| f.as_raw_fd()) {
                        daemon.clients.insert(rfd, fclient);
                        eprintln!("ranchd: forge: worker started (pipe fd {rfd})");
                    }
                    Some(tx)
                }
                Err(e) => {
                    eprintln!("ranchd: forge: disabled: {e}");
                    None
                }
            }
        } else {
            eprintln!("ranchd: forge: disabled (no forge_api_key in daemon.toml)");
            None
        };
        daemon.forge_tx = forge_tx;
        Ok(daemon)
    }

    /// Cold start: full construction + Tier-1 restore from state.json.
    fn new() -> Result<Daemon, String> {
        let socket_path = default_socket_path();
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        if socket_path.exists() {
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
        let v = state_value(&self.sessions, &self.pi_agents);
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
        for (sid, s) in &self.sessions {
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
        let state = state_value(&self.sessions, &self.pi_agents);
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
                            cwd: cwd.clone(),
                        },
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
                            cwd: cwd.clone(),
                        },
                    );
                    chat_ids.push(pid);
                    let is_pi = fsid.is_nil();
                    if is_pi {
                        // respawn the local rpc child in the recorded cwd
                        let dir = cwd.clone().unwrap_or_else(home_dir_string);
                        // switch_session races pi's own init on a brand-new
                        // child; give it a moment to boot the RPC loop
                        std::thread::sleep(std::time::Duration::from_millis(300));
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
                                if let Some(sf) = &pi_file {
                                    if let Some(agent) = self.pi_agents.get(&pid) {
                                        if let Err(e) = agent.switch_session(sf) {
                                            eprintln!("ranchd: pi switch_session failed: {e}");
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
    fn resnap(&mut self, sid: &Uuid) {
        if let Some(s) = self.sessions.get(sid) {
            if let Some(mut snap) = snapshot_session(s) {
                let recipients: Vec<RawFd> = self
                    .clients
                    .iter()
                    .filter(|(_, c)| c.attach == Some(*sid))
                    .map(|(f, _)| *f)
                    .collect();
                for rfd in recipients {
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
            // PTY/pi fds ride through the exec (never returns on success)
            Frame::Upgrade {} => {
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
                ..
            } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let Ok(pid) = Uuid::parse_str(pane) else {
                    return;
                };
                if let Some(s) = self.sessions.get(&sid) {
                    if let Some(cp) = s.chats.get(&pid) {
                        if cp.forge_sid.is_nil() {
                            // local pi backing: prompt the child directly
                            if let Some(lp) = self.pi_agents.get(&pid) {
                                if let Some(pipe_w) = &self.forge_pipe_w {
                                    if let Err(e) = lp.prompt(pipe_w, &text.clone()) {
                                        eprintln!("ranchd: pi prompt failed: {e}");
                                    }
                                }
                            }
                        } else if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::Send {
                                pane: pid,
                                forge_sid: cp.forge_sid,
                                text: text.clone(),
                            });
                        }
                    }
                }
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
            // forge worker agent-status: resolve + broadcast
            Frame::Meta {
                session,
                pane,
                kind,
                status,
            } if session.is_empty() && (kind == "agent" || kind == "model") => {
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
                    let out = Frame::Meta {
                        session: sid.to_string(),
                        pane: Some(pid.to_string()),
                        kind: kind.clone(),
                        status: status.clone(),
                    };
                    let recipients: Vec<RawFd> = self
                        .clients
                        .iter()
                        .filter(|(_, c)| c.attach == Some(sid))
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
                        },
                    );
                }
            }
            Frame::Attach {
                id,
                client,
                session,
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
                forge_session,
            } => {
                let kind = kind.clone().unwrap_or_else(|| "shell".into());
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
                            .map(|cfg| forge::create_forge_session(cfg, &name, cwd.as_deref()))
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
                                    cwd: cwd.clone(),
                                },
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
                                lp.kill();
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
                                    cwd: None,
                                },
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
                                cwd: None,
                            },
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
                            lp.kill();
                        }
                        if let Some(tx) = &self.forge_tx {
                            let _ = tx.send(forge::ForgeJob::Unwatch { pane: pid });
                        }
                    }
                    let dead = s.remove_pane_everywhere(&pid.to_string());
                    s.apply_sizes();
                    if dead || s.panes.is_empty() {
                        need_snap = None;
                        eprintln!("ranchd: last window closed, killing session {}", s.name);
                        self.sessions.remove(&sid);
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
        let mut daemon = match Daemon::inherit(&args[2], listen_fd) {
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
        run(daemon);
        return;
    }
    if args.len() >= 2 && args[1] == "upgrade" {
        eprintln!("ranchd: `upgrade` is sent as a frame to the running daemon (ranch upgrade)");
        std::process::exit(2);
    }
    let mut daemon = match Daemon::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ranchd: {e}");
            std::process::exit(1);
        }
    };
    run(daemon);
}

fn run(daemon: Daemon) {
    let mut daemon = daemon;
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
                        decoder: Decoder::new(),
                        name: format!("cli-{fd}"),
                        attach: None,
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

        // --- file watches: external edits to open editor files (M10 ph3).
        // Cheap metadata stats on a handful of files; a changed mtime
        // pushes FileChanged to the watching client (baseline refreshes
        // on FileRead/FileWriteOk so our own saves stay quiet). ---
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
                        eprintln!("ranchd: pane {pid} ({}:{}) child exited", s.name, s.id);
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
            if gone_last || s.panes.is_empty() {
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
    let _ = std::fs::remove_file(&daemon.socket_path);
}
