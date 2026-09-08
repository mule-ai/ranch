//! `ranchd` — the Ranch local daemon.
//!
//! Owns every terminal session on this machine: PTYs, ghostty-vt
//! emulators, and the state that keeps them alive after every client
//! disconnects. Clients (the `ranch` CLI today, the mobile app in M3)
//! speak the ranch-protocol frame set over a unix socket; the same frames
//! later flow through the Supabase relay unchanged (SPEC §4, PROTOCOL.md).
//!
//! M1 scope decisions (SPEC §12):
//! - one *visible* pane per session at a time (the active pane);
//!   splits create additional panes you switch between — visual
//!   side-by-side layout is post-M1
//! - scrollback is a heuristic ring (lines observed scrolling off the
//!   top of the formatted screen)
//! - sessions do not survive a daemon restart (state.json is written for
//!   observability; true durability is post-M1)
//! - single-threaded blocking poll loop: unix socket, clients, pty masters

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

const TICK_MS: i32 = 30;
const SCROLLBACK_CAP: usize = 2000;
const POLLIN: i16 = 0x001;
const POLLHUP: i16 = 0x00200;
const POLLERR: i16 = 0x00004;

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
}

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
}

struct Client {
    stream: UnixStream,
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

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "ranch".into())
}

/// Spawn a new shell pane into a session (full-session size in M1).
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
    let mut snaps = Vec::new();
    let mut active_seq: u64 = 0;
    for (pid, p) in &s.panes {
        let lines = p.vt.screen();
        let (cx, cy, vis) = p.vt.cursor();
        let snap = PaneSnap {
            id: pid.to_string(),
            cols: s.size.0,
            rows: s.size.1,
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
        layout: Layout::Leaf { pane: s.active.to_string() },
        active_pane: s.active.to_string(),
        panes: snaps,
        meta: vec![],
    })
}

/// Serialize + chunk a frame and write all bytes to the client stream.
fn send_frame(c: &mut Client, frame: &Frame) {
    let cid = Uuid::new_v4().to_string();
    for line in ranch_protocol::encode_frame(frame, &cid) {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let mut off = 0usize;
        while off < bytes.len() {
            match c.stream.write(&bytes[off..]) {
                Ok(0) => break,
                Ok(w) => off += w,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("ranchd: write: {e}");
                    break;
                }
            }
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

        eprintln!("ranchd: machine={}", hostname());
        Ok(Daemon {
            machine: hostname(),
            socket_path,
            listener,
            sessions: BTreeMap::new(),
            clients: BTreeMap::new(),
            state_path,
        })
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
                            for p in s.panes.values_mut() {
                                p.vt.resize(*cols, *rows);
                                p.prev_screen.clear();
                                p.dirty = true;
                            }
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
                let name = name.clone().unwrap_or_else(|| format!("s{}", id.as_simple()));
                let mut s = Session {
                    id,
                    name: name.clone(),
                    kind: "shell".into(),
                    panes: BTreeMap::new(),
                    active: Uuid::nil(),
                    size: (80, 24),
                };
                match spawn_pane(&mut s) {
                    Ok(pid) => {
                        eprintln!("ranchd: created session {name} ({id}) pane {pid}");
                        let ack = Frame::SessionsAck {
                            req_id: req_id.clone(),
                            session: id.to_string(),
                            pane: pid.to_string(),
                        };
                        self.sessions.insert(id, s);
                        self.write_state();
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
                    if let Some(s2) = self.sessions.get_mut(&sid) {
                        s2.name = name.clone();
                    }
                    self.write_state();
                }
            }
            Frame::SessionsKill { session } => {
                if let Some(sid) = self.resolve_session(session).map(|s| s.id) {
                    if let Some(s) = self.sessions.remove(&sid) {
                        eprintln!("ranchd: killed session {}", s.name);
                    }
                    self.write_state();
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
                pane: _pane,
                dir: _dir,
            } => {
                // M1: a split adds a second pane to the session; both panes
                // run at the full session size and the client switches
                // between them (visual side-by-side is post-M1).
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let new_pane = self.sessions.get_mut(&sid).and_then(|s| spawn_pane(s).ok());
                if let Some(pid) = new_pane {
                    self.write_state();
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
            Frame::PaneKill { session, pane } => {
                let sid = match self.resolve_session(session).map(|s| s.id) {
                    Some(s) => s,
                    None => return,
                };
                let pid = match Uuid::parse_str(pane) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                if let Some(s) = self.sessions.get_mut(&sid) {
                    if let Some(p) = s.panes.remove(&pid) {
                        drop(p);
                    }
                    if s.active == pid {
                        s.active = s
                            .panes
                            .keys()
                            .next()
                            .copied()
                            .unwrap_or_else(Uuid::new_v4);
                    }
                    if s.panes.is_empty() {
                        self.sessions.remove(&sid);
                    }
                }
                self.write_state();
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
            fds.push(pollfd {
                fd: c.stream.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            });
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
                        stream,
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
            let r = match c.stream.read(&mut buf) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("ranchd: client {fd} read: {e}");
                    gone.push(*fd);
                    continue;
                }
            };
            if r == 0 {
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
                    let new_screen = p.vt.screen();
                    let (cx, cy, vis) = p.vt.cursor();
                    // scrollback heuristic: a line scrolled off the top when
                    // most other lines are unchanged and the top line moved.
                    if new_screen.len() == p.prev_screen.len()
                        && !p.prev_screen.is_empty()
                        && !p.prev_screen[0].is_empty()
                        && p.prev_screen[0] != new_screen[0]
                    {
                        let same = (1..new_screen.len())
                            .filter(|&i| new_screen[i] == p.prev_screen[i])
                            .count();
                        if same > new_screen.len() / 2 {
                            p.scrollback.push_back(p.prev_screen[0].clone());
                            while p.scrollback.len() > SCROLLBACK_CAP {
                                p.scrollback.pop_front();
                            }
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
                        cols: s.size.0,
                        rows: s.size.1,
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
