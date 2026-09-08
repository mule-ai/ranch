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

mod relay;

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use libc::{c_int, pollfd, SIGINT, WNOHANG};
use ranch_protocol::{Cursor, Decoder, Frame, Layout, PaneSnap, SessionMeta};
use ranch_vt::Vt;
use uuid::Uuid;

const TICK_MS: i32 = 8;
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
    let (h, mi, s) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
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

struct Session {
    id: Uuid,
    name: String,
    kind: String,
    panes: BTreeMap<Uuid, Pane>,
    active: Uuid,
    size: (u16, u16),
    /// Binary split tree. The root leaf is the first pane; PaneSplit
    /// replaces the split target's leaf with a new node. `pct` is the
    /// percent of space given to `a`.
    layout: Layout,
}

impl Session {
    /// Replace the leaf `target` with a split whose second child is
    /// `new_id`. Returns true when found.
    fn split_leaf(&mut self, target: &str, new_id: &str, dir: u8) -> bool {
        fn walk(l: &mut Layout, target: &str, new_id: &str, dir: u8) -> bool {
            match l {
                Layout::Leaf { pane } if pane == target => {
                    let nb = Layout::Leaf { pane: new_id.to_string() };
                    let na = std::mem::replace(l, Layout::Leaf { pane: String::new() });
                    *l = Layout::Split { dir, a: Box::new(na), b: Box::new(nb), pct: 50 };
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
                        let promoted = std::mem::replace(keep, Layout::Leaf { pane: String::new() });
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
        walk(&self.layout, 0, 0, self.size.0, self.size.1, &mut out);
        out
    }

    /// Apply computed sizes to every pane's PTY + VT (with SIGWINCH).
    fn apply_sizes(&mut self) {
        // bootstrap: root leaf may still point at the nil placeholder
        if let Layout::Leaf { pane } = &self.layout {
            if Uuid::parse_str(&pane).map(|u| u.is_nil()).unwrap_or(true) {
                if let Some(first) = self.panes.keys().next().copied() {
                    self.layout = Layout::Leaf { pane: first.to_string() };
                }
            }
        }
        let sizes = self.pane_sizes();
        for (pid, cols, rows) in sizes {
            if let Some(p) = self.panes.get_mut(&pid) {
                if p.vt.dims() != (cols, rows) {
                    p.vt.resize(cols, rows);
                    let win = Winsize { ws_row: rows, ws_col: cols, ..Default::default() };
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
        let Some((ndir, a_leaf)) = find(&self.layout, &pref) else {
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
        let a_is_target = is_a(&self.layout, &pref).unwrap_or(false);
        let sizes = self.pane_sizes();
        let axis = if dir == 1 { self.size.0 } else { self.size.1 };
        let a_size = Uuid::parse_str(&a_leaf)
            .ok()
            .and_then(|pid| sizes.iter().find(|(id, _, _)| *id == pid))
            .map(|(_, c, r)| if dir == 1 { *c } else { *r })
            .unwrap_or(axis / 2);
        let new_a = (a_size as i32
            + if a_is_target { delta as i32 } else { -(delta as i32) })
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
        set_pct(&mut self.layout, &pref, dir, new_pct);
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
}

// ---------- helpers ----------

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/root"))
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

/// Spawn a new shell pane. The CALLER must insert the layout split
/// (via `Session::split_leaf`) before or after; the PTY is sized with
/// `session.size` and `apply_sizes` fixes it after the split is made.
fn spawn_pane(session: &mut Session) -> Result<Uuid, String> {
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
                b"TERM\0".as_ptr() as *const i8,
                b"xterm-256color\0".as_ptr() as *const i8,
                1,
            );
            let shell = default_shell();
            let shell_c = shell.as_ptr();
            let argv: [*const u8; 2] = [shell_c, std::ptr::null()];
            execvp(shell_c, argv.as_ptr());
            std::process::exit(1);
        }
    }
    unsafe {
        libc::close(aslave);
    }
    let id = Uuid::new_v4();
    session.panes.insert(
        id,
        Pane {
            kind: "shell".into(),
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
            cursor: Some(Cursor { x: cx, y: cy, visible: vis }),
        };
        if *pid == s.active {
            active_seq = p.seq;
        }
        snaps.push(snap);
    }
    Some(Frame::Snapshot {
        id: Uuid::new_v4().to_string(),
        client: String::new(), // filled per-recipient
        session: s.id.to_string(),
        seq: active_seq,
        layout: s.layout.clone(),
        active_pane: s.active.to_string(),
        panes: snaps,
        meta: vec![],
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
    fn new() -> Result<Daemon, String> {
        let socket_path = default_socket_path();
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).ok();
        }
        let listener =
            UnixListener::bind(&socket_path).map_err(|e| format!("bind {socket_path:?}: {e}"))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).ok();
        let state_path = socket_path.parent().unwrap().join("state.json");

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
        Ok(daemon)
    }

    fn write_state(&self) {
        let v: serde_json::Value = serde_json::json!({
            "machine": self.machine,
            "sessions": self.sessions.iter().map(|(id, s)| serde_json::json!({
                "id": id.to_string(),
                "name": s.name,
                "kind": s.kind,
                "active": s.active.to_string(),
                "size": [s.size.0, s.size.1],
                "panes": s.panes.iter().map(|(pid, p)| serde_json::json!({
                    "id": pid.to_string(),
                    "kind": p.kind,
                    "dead": p.dead,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        if let Ok(json) = serde_json::to_string_pretty(&v) {
            std::fs::write(&self.state_path, json).ok();
        }
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
            Frame::Hello { id, client, .. } => {
                eprintln!("ranchd: hello from {client} ({id})");
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
            Frame::Attach { id, client, session, .. } => {
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
            Frame::Input { session, pane, data, .. } => {
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
            Frame::Resize { session, cols, rows, .. } => {
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
            Frame::SessionsCreate { req_id, name } => {
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
                    kind: "shell".into(),
                    panes: BTreeMap::new(),
                    active: Uuid::nil(),
                    size: (80, 24),
                    layout: Layout::Leaf { pane: Uuid::nil().to_string() },
                };
                    match spawn_pane(&mut s) {
                    Ok(pid) => {
                        s.layout = Layout::Leaf { pane: pid.to_string() };
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
                            kind: "shell".into(),
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
                        eprintln!("ranchd: killed session {}", s.name);
                    }
                    self.write_state();
                    self.mirror(relay::RelayOut::DeleteSession { id: sid.to_string() });
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
            } => {
                // Real split: replace the target leaf with a split node,
                // spawn the new pane, then re-apply sizes (sibling shrinks).
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let dir = *dir.min(&1); // 0 = top/bottom, 1 = left/right
                let new_pane = self.sessions.get_mut(&sid).and_then(|s| {
                    let target = match Uuid::parse_str(pane) {
                        Ok(p) if s.panes.contains_key(&p) => p,
                        _ => s.active,
                    };
                    match spawn_pane(s) {
                        Ok(pid) => {
                            s.split_leaf(&target.to_string(), &pid.to_string(), dir);
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
            Frame::PaneResize { session, pane, dir, delta } => {
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
                    s.remove_leaf(&pid.to_string());
                    if s.active == pid {
                        s.active = s
                            .panes
                            .keys()
                            .next()
                            .copied()
                            .unwrap_or_else(Uuid::new_v4);
                    }
                    s.apply_sizes();
                    if s.panes.is_empty() {
                        need_snap = None;
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
            Frame::Hb | Frame::Meta { .. } | Frame::Error { .. } | Frame::Chunk { .. }
            | Frame::HelloOk { .. }
            | Frame::Snapshot { .. }
            | Frame::Update { .. }
            | Frame::SessionsAck { .. }
            | Frame::Scrollback { .. } => {
                // client->daemon direction: not expected; ignore
            }
        }
    }
}

// ---------- main loop ----------

static RUNNING: AtomicUsize = AtomicUsize::new(1);
extern "C" fn on_signal(_sig: c_int) {
    // SAFETY: a lock-free word store is async-signal-safe on x86-64.
    RUNNING.store(0, Ordering::SeqCst);
}

fn main() {
    let mut daemon = match Daemon::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ranchd: {e}");
            std::process::exit(1);
        }
    };

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

        let timeout: c_int = if RUNNING.load(Ordering::SeqCst) != 0 { TICK_MS } else { -1 };
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
                let r = unsafe { libc::read(pin.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
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
                            let r =
                                unsafe { libc::read(p.master, buf.as_mut_ptr() as *mut _, buf.len()) };
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

        // --- child liveness + tick: coalesce dirty panes into updates ---
        let mut updates: Vec<Frame> = Vec::new();
        for (_sid, s) in daemon.sessions.iter_mut() {
            let alive = s.panes.values().any(|pp| !pp.dead);
            for (pid, p) in s.panes.iter_mut() {
                if !p.dead {
                    let mut status: c_int = 0;
                    let w = unsafe { waitpid(p.child, &mut status, WNOHANG) };
                    if w == p.child {
                        p.dead = true;
                        eprintln!("ranchd: pane {pid} ({}:{}) child exited", s.name, s.id);
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
                        cursor: Some(Cursor { x: cx, y: cy, visible: vis }),
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

        // --- shutdown ---
        if RUNNING.load(Ordering::SeqCst) == 0 {
            break;
        }
    }

    eprintln!("ranchd: shutting down");
    let _ = std::fs::remove_file(&daemon.socket_path);
}
