//! `ranch` — CLI client for the ranch daemon.
//!
//! M1 commands:
//!   ranch new [name]          create a shell session
//!   ranch ls                  list sessions
//!   ranch attach <ref>        attach TUI to a session (Ctrl-C to detach)
//!   ranch kill <ref>          kill a session
//!   ranch rename <ref> <name> rename a session
//!   ranch split <ref>         add a pane to a session
//!   ranch switch <ref> <pane> switch the session's active pane
//!
//! Socket: $RANCH_SOCKET or ~/.local/state/ranch/daemon.sock

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crossterm::event::{
    Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read as read_event,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame as RFrame;
use ratatui::Terminal;
use uuid::Uuid;

use ranch_protocol::{Decoder, Frame};

fn socket_path() -> std::path::PathBuf {
    std::env::var("RANCH_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local/state/ranch/daemon.sock"))
                .unwrap_or_default()
        })
}

fn die(msg: &str) -> ! {
    eprintln!("ranch: {msg}");
    std::process::exit(1)
}

/// Encode a frame (chunking if needed) and write all lines to the stream.
fn send_frame(stream: &mut UnixStream, frame: &Frame) -> Result<(), std::io::Error> {
    let lines = ranch_protocol::encode_frame(frame, &Uuid::new_v4().to_string());
    let mut bytes = Vec::new();
    for line in &lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    stream.write_all(&bytes)
}

fn connect() -> UnixStream {
    let path = socket_path();
    UnixStream::connect(&path).unwrap_or_else(|e| die(&format!("connect {path:?}: {e}")))
}

fn hello(stream: &mut UnixStream, client: &str) {
    let hello = Frame::Hello {
        id: Uuid::new_v4().to_string(),
        client: client.into(),
        caps: vec![],
    };
    if send_frame(stream, &hello).is_err() {
        die("failed to send hello");
    }
}

/// Connect, hello, send one command, wait for the matching response, return it.
fn one_shot<F: FnOnce(&str) -> Frame>(send: F, expect: &str, on_resp: impl FnOnce(&Frame)) -> Frame {
    let mut stream = connect();
    hello(&mut stream, "cli");
    let req_id = Uuid::new_v4().to_string();
    let frame = send(&req_id);
    if send_frame(&mut stream, &frame).is_err() {
        die(&format!("failed to send {expect} request"));
    }

    let mut decoder = Decoder::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            die("daemon closed the connection");
        }
        for f in decoder.feed(&buf[..n]) {
            if matches!(f, Frame::HelloOk { .. }) {
                continue;
            }
            if frame_matches(&f, expect) {
                on_resp(&f);
                return f;
            }
        }
    }
}

fn frame_matches(f: &Frame, tag: &str) -> bool {
    match (f, tag) {
        (Frame::SessionsAck { .. }, "ack") => true,
        (Frame::Error { .. }, "ack") => true,
        (Frame::Meta { .. }, "meta") => true,
        _ => false,
    }
}

// ---------- commands ----------

fn cmd_new(name: Option<String>) {
    one_shot(
        |req_id| Frame::SessionsCreate {
            req_id: req_id.to_string(),
            name,
        },
        "ack",
        |f| match f {
            Frame::SessionsAck { session, pane, .. } => println!("session {session} pane {pane}"),
            Frame::Error { message, .. } => eprintln!("error: {message}"),
            _ => {}
        },
    );
}

fn cmd_ls() {
    let mut stream = connect();
    hello(&mut stream, "cli");
    let mut decoder = Decoder::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            die("daemon closed the connection");
        }
        for f in decoder.feed(&buf[..n]) {
            if let Frame::HelloOk { machine, sessions, .. } = f {
                println!("machine: {machine}");
                if sessions.is_empty() {
                    println!("(no sessions)");
                }
                for s in &sessions {
                    let active = s
                        .panes
                        .iter()
                        .position(|p| p == &s.active_pane)
                        .map_or("?".into(), |i| i.to_string());
                    println!(
                        "  {:<10} {:>4} panes  active#{}  {}",
                        s.name,
                        s.panes.len(),
                        active,
                        s.id
                    );
                }
                return;
            }
        }
    }
}

fn cmd_kill(ref_: &str) {
    one_shot(
        |_req_id| Frame::SessionsKill {
            session: ref_.to_string(),
        },
        "meta",
        |f| match f {
            Frame::Meta { status, .. } => println!("killed ({status:?})"),
            Frame::Error { message, .. } => eprintln!("error: {message}"),
            _ => {}
        },
    );
}

fn cmd_rename(ref_: &str, new_name: &str) {
    let mut stream = connect();
    hello(&mut stream, "cli");
    let f = Frame::SessionsRename {
        session: ref_.to_string(),
        name: new_name.to_string(),
    };
    if send_frame(&mut stream, &f).is_err() {
        die("failed to send rename");
    }
    // The daemon responds with an Error frame on failure; nothing on success.
    let mut decoder = Decoder::new();
    let mut buf = [0u8; 4096];
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    if stream.read(&mut buf).unwrap_or(0) > 0 {
        for fr in decoder.feed(&buf) {
            if let Frame::Error { message, .. } = fr {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        }
    }
    println!("renamed to {new_name}");
}

fn cmd_split(ref_: &str) {
    one_shot(
        |req_id| Frame::PaneSplit {
            req_id: req_id.to_string(),
            session: ref_.to_string(),
            pane: String::new(),
            dir: 0,
        },
        "ack",
        |f| match f {
            Frame::SessionsAck { session, pane, .. } => {
                println!("new pane {pane} in session {session}")
            }
            Frame::Error { message, .. } => eprintln!("error: {message}"),
            _ => {}
        },
    );
}

fn cmd_switch(ref_: &str, pane: &str) {
    let mut stream = connect();
    hello(&mut stream, "cli");
    let f = Frame::SessionsSelect {
        session: ref_.to_string(),
        pane: pane.to_string(),
    };
    if send_frame(&mut stream, &f).is_err() {
        die("failed to send select");
    }
    println!("switched to pane {pane}");
}

// ---------- attach TUI ----------

struct Screen {
    lines: Vec<String>,
    cols: u16,
    rows: u16,
    cursor: (u16, u16, bool), // x, y, visible
}

impl Screen {
    fn reset(cols: u16, rows: u16) -> Self {
        Screen {
            lines: vec![String::new(); rows as usize],
            cols,
            rows,
            cursor: (0, 0, true),
        }
    }
    /// Blank the local screen (switching sessions; a snapshot will refill it).
    fn reset_blank(&mut self) {
        self.lines = vec![String::new(); self.rows as usize];
        self.cursor = (0, 0, true);
    }
}

/// Client-side state for one pane: screen contents + cursor, at the
/// pane's own (cols, rows) reported by the daemon.
#[derive(Default, Clone)]
struct PaneView {
    lines: Vec<String>,
    cols: u16,
    rows: u16,
    cursor: Option<(u16, u16, bool)>,
}

impl PaneView {
    fn apply_snapshot(&mut self, snap: &ranch_protocol::PaneSnap) {
        self.cols = snap.cols;
        self.rows = snap.rows;
        self.lines = snap.lines.clone();
        self.cursor = snap.cursor.map(|c| (c.x, c.y, c.visible));
    }
    fn apply_update(&mut self, cols: u16, rows: u16, rows_upd: &[(u16, String)], cursor: &Option<ranch_protocol::Cursor>) {
        self.cols = cols;
        self.rows = rows;
        self.lines.resize(rows as usize, String::new());
        for (idx, text) in rows_upd {
            let idx = *idx as usize;
            if idx < self.lines.len() {
                self.lines[idx] = text.clone();
            }
        }
        if let Some(c) = cursor {
            self.cursor = Some((c.x, c.y, c.visible));
        }
    }
}

fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let mods = key.modifiers;
    let has_ctrl = mods.contains(KeyModifiers::CONTROL);
    let has_alt = mods.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) => {
            if has_ctrl {
                match c {
                    'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
                    ' ' => Some(vec![0]),
                    _ => None,
                }
            } else if has_alt {
                let mut b = vec![0x1b];
                b.extend(c.to_string().into_bytes());
                Some(b)
            } else {
                let mut b = String::new();
                b.push(c);
                Some(b.into_bytes())
            }
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        _ => None,
    }
}

/// Find the pane rect geometrically adjacent to `cur` in `dirs` from the
/// rect list produced by the layout walk. Returns the pane id or None.
#[allow(dead_code)]
fn neighbor_pane(
    rects: &[(String, ratatui::layout::Rect)],
    cur: &str,
    dir: u8, // 0=up 1=down 2=left 3=right
) -> Option<String> {
    let (_, cr) = rects.iter().find(|(p, _)| p == cur)?;
    let ccx = cr.x + cr.width / 2;
    let ccy = cr.y + cr.height / 2;
    let mut best: Option<(u32, &String)> = None;
    for (pid, r) in rects {
        if pid == cur {
            continue;
        }
        let bx = r.x + r.width / 2;
        let by = r.y + r.height / 2;
        let (dx, dy) = (bx as i32 - ccx as i32, by as i32 - ccy as i32);
        let ok = match dir {
            0 => dy < 0 && dx.abs() <= dy.abs() * 2,
            1 => dy > 0 && dx.abs() <= dy.abs() * 2,
            2 => dx < 0 && dy.abs() <= dx.abs() * 2,
            _ => dx > 0 && dy.abs() <= dx.abs() * 2,
        };
        if ok {
            let dist = (dx * dx + dy * dy) as u32;
            if best.is_none() || dist < best.unwrap().0 {
                best = Some((dist, pid));
            }
        }
    }
    best.map(|(_, p)| p.clone())
}

fn refresh_sessions(stream: &mut UnixStream) {
    let hello = Frame::Hello {
        id: Uuid::new_v4().to_string(),
        client: "cli".into(),
        caps: vec![],
    };
    send_frame(stream, &hello).ok();
}

/// Interactive session manager — plain `ranch` with a TTY. Lists sessions,
/// create, kill, rename, and attach. Returns the session ref to attach to,
/// or None to exit.
fn cmd_dashboard() -> Option<String> {
    crossterm::terminal::enable_raw_mode().ok();
    execute!(std::io::stdout(), EnterAlternateScreen).ok();
    let restore = || {
        crossterm::terminal::disable_raw_mode().ok();
        execute!(std::io::stdout(), LeaveAlternateScreen).ok();
    };

    let mut stream = connect();
    stream.set_nonblocking(true).ok();
    let client_id = format!("cli-{}", Uuid::new_v4().as_simple());
    hello(&mut stream, &client_id);

    let mut decoder = Decoder::new();
    let mut buf = [0u8; 65536];
    let mut sessions: Vec<ranch_protocol::SessionMeta> = vec![];
    let mut sel: usize = 0;
    let mut status = String::new();
    // Some(kind) while an inline input is active: "new" | "rename"
    let mut input: Option<&'static str> = None;
    let mut input_text = String::new();
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .expect("failed to init terminal");

    loop {
        // drain socket
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    restore();
                    die("daemon closed the connection");
                }
                Ok(n) => {
                    for f in decoder.feed(&buf[..n]) {
                        match f {
                            Frame::HelloOk { sessions: ss, .. } => {
                                sessions = ss;
                                if sel >= sessions.len() {
                                    sel = sessions.len().saturating_sub(1);
                                }
                            }
                            Frame::SessionsAck { session, .. } => {
                                restore();
                                drop(term);
                                return Some(session);
                            }
                            Frame::Error { message, .. } => {
                                status = format!("error: {message}");
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // draw
        let sess_ref = &sessions;
        let sel_ref = sel;
        let status_ref = &status;
        let input_ref = input;
        let input_text_ref = &input_text;
        let _ = term.draw(|f: &mut RFrame| {
            let area = f.area();
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(
                    " ranch — {} session{} on this machine",
                    sess_ref.len(),
                    if sess_ref.len() == 1 { "" } else { "s" }
                ),
                Style::default().add_modifier(Modifier::REVERSED),
            )));
            lines.push(Line::raw(""));
            if sess_ref.is_empty() {
                lines.push(Line::raw("  no sessions yet — press c to create one"));
            }
            for (i, s2) in sess_ref.iter().enumerate() {
                let cur = i == sel_ref;
                let style = if cur {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let marker = if cur { ">" } else { " " };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{marker} {:<24} {:>7} {:>3} pane{}",
                        s2.name,
                        s2.kind,
                        s2.panes.len(),
                        if s2.panes.len() == 1 { "" } else { "s" }
                    ),
                    style,
                )));
            }
            lines.push(Line::raw(""));
            lines.push(Line::raw(
                " enter: attach   c: new   n: new (named)   k: kill   r: rename   q: quit",
            ));
            if !status_ref.is_empty() {
                lines.push(Line::from(Span::styled(
                    status_ref.clone(),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            if let Some(kind) = input_ref {
                let label = match kind {
                    "new" => "new session name (empty = auto): ",
                    _ => "rename to: ",
                };
                lines.push(Line::from(Span::styled(
                    format!("{label}{}\u{2588}", input_text_ref),
                    Style::default().add_modifier(Modifier::REVERSED),
                )));
            }
            f.render_widget(
                Paragraph::new(lines),
                Rect::new(0, 0, area.width, area.height),
            );
        });

        // wait for one event (timeout keeps the socket drain flowing)
        if !crossterm::event::poll(std::time::Duration::from_millis(120)).unwrap_or(false) {
            continue;
        }
        let ev = match crossterm::event::read() {
            Ok(e) => e,
            Err(_) => continue,
        };
        if let CEvent::Key(key) = ev {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(kind) = input {
                match key.code {
                    KeyCode::Esc => {
                        input = None;
                        input_text.clear();
                    }
                    KeyCode::Enter => {
                        let text = input_text.trim().to_string();
                        match kind {
                            "new" => {
                                let f = Frame::SessionsCreate {
                                    req_id: Uuid::new_v4().to_string(),
                                    name: if text.is_empty() { None } else { Some(text) },
                                };
                                send_frame(&mut stream, &f).ok();
                                input = None;
                                input_text.clear();
                                // SessionsAck handler attaches
                            }
                            _ => {
                                if let Some(s) = sessions.get(sel) {
                                    if !text.is_empty() {
                                        let f = Frame::SessionsRename {
                                            session: s.id.clone(),
                                            name: text,
                                        };
                                        send_frame(&mut stream, &f).ok();
                                        refresh_sessions(&mut stream);
                                    }
                                }
                                input = None;
                                input_text.clear();
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input_text.pop();
                    }
                    KeyCode::Char(c) => input_text.push(c),
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    restore();
                    drop(term);
                    return None;
                }
                KeyCode::Up | KeyCode::Char('k') if !matches!(key.modifiers, KeyModifiers::CONTROL) => {
                    if sel > 0 {
                        sel -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if sel + 1 < sessions.len() {
                        sel += 1;
                    }
                }
                KeyCode::Char('c') => {
                    let f = Frame::SessionsCreate {
                        req_id: Uuid::new_v4().to_string(),
                        name: None,
                    };
                    send_frame(&mut stream, &f).ok();
                    // SessionsAck handler attaches
                }
                KeyCode::Char('n') => {
                    input = Some("new");
                    input_text.clear();
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Ctrl-K: also kill, symmetric with j/k nav quirk
                    if let Some(s) = sessions.get(sel) {
                        let f = Frame::SessionsKill { session: s.id.clone() };
                        send_frame(&mut stream, &f).ok();
                        refresh_sessions(&mut stream);
                    }
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Ctrl-D on the dashboard: kill too (documented as k? use x)
                    if let Some(s) = sessions.get(sel) {
                        let f = Frame::SessionsKill { session: s.id.clone() };
                        send_frame(&mut stream, &f).ok();
                        refresh_sessions(&mut stream);
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(s) = sessions.get(sel) {
                        let f = Frame::SessionsKill { session: s.id.clone() };
                        send_frame(&mut stream, &f).ok();
                        status = format!("killed {}", s.name);
                        refresh_sessions(&mut stream);
                    }
                }
                KeyCode::Char('r') => {
                    if !sessions.is_empty() {
                        input = Some("rename");
                        input_text.clear();
                    }
                }
                KeyCode::Enter => {
                    if let Some(s) = sessions.get(sel) {
                        restore();
                        drop(term);
                        return Some(s.id.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

fn cmd_attach(ref_: &str) {
    let mut stream = connect();
    stream.set_nonblocking(true).ok();

    let client_id = format!("cli-{}", Uuid::new_v4().as_simple());
    hello(&mut stream, &client_id);

    let attach = Frame::Attach {
        id: Uuid::new_v4().to_string(),
        client: client_id,
        session: ref_.to_string(),
        pane: None,
    };
    if send_frame(&mut stream, &attach).is_err() {
        die("failed to send attach");
    }

    // TUI setup
    execute!(std::io::stdout(), EnterAlternateScreen).ok();
    enable_raw_mode().ok();
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .expect("failed to init terminal");

    let mut screen = Screen::reset(80, 24);
    let mut session_id = String::new();
    let mut panes: Vec<String> = vec![];
    let mut active_pane = String::new();
    // per-pane client state for split rendering
    let mut pane_views: std::collections::HashMap<String, PaneView> =
        std::collections::HashMap::new();
    let mut layout: Option<ranch_protocol::Layout> = None;
    let mut decoder = Decoder::new();
    let mut buf = [0u8; 65536];
    let got_snapshot = std::cell::Cell::new(false);
    let sent_resize = std::cell::Cell::new(false);
    // tmux prefix state + session list for prefix-n/p + picker
    let prefix_mode = std::cell::Cell::new(false);
    let session_list: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(vec![]));
    let picker = std::cell::Cell::new(false);
    let picker_sel = std::cell::Cell::new(0usize);
    #[derive(Clone, Copy, PartialEq)]
    enum Prompt { Rename, Command }
    let prompt = std::cell::Cell::new(None::<Prompt>);
    let mut prompt_input = String::new();
    let mut sessions_meta: Vec<ranch_protocol::SessionMeta> = vec![];
    let _ = &mut sessions_meta;
    let _ = &picker_sel;

    let (cols, rows) = size().unwrap_or((80, 24));

    let restore = || {
        disable_raw_mode().ok();
        execute!(std::io::stdout(), LeaveAlternateScreen).ok();
    };

    loop {
        // drain socket
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    restore();
                    drop(term);
                    die("daemon closed the connection");
                }
                Ok(n) => {
                    for f in decoder.feed(&buf[..n]) {
                        match f {
                            Frame::HelloOk { sessions, .. } => {
                                *session_list.borrow_mut() =
                                    sessions.iter().map(|s| s.id.clone()).collect();
                                sessions_meta.clone_from(&sessions);
                            }
                            Frame::Snapshot {
                                session,
                                panes: panes_snap,
                                active_pane: ap,
                                layout: ly,
                                ..
                            } => {
                                if session_id.is_empty() || session_id != session {
                                    session_id = session.clone();
                                }
                                active_pane = ap.clone();
                                panes = panes_snap.iter().map(|p| p.id.clone()).collect();
                                layout = Some(ly);
                                pane_views.clear();
                                for ps in &panes_snap {
                                    let mut pv = PaneView::default();
                                    pv.apply_snapshot(ps);
                                    pane_views.insert(ps.id.clone(), pv);
                                }
                                if let Some(p) = panes_snap
                                    .iter()
                                    .find(|p| p.id == active_pane)
                                    .or(panes_snap.first())
                                {
                                    screen.cols = p.cols;
                                    screen.rows = p.rows;
                                    screen.lines = p.lines.clone();
                                    screen.cursor = p
                                        .cursor
                                        .map(|c| (c.x, c.y, c.visible))
                                        .unwrap_or((0, 0, true));
                                }
                                got_snapshot.set(true);
                                // tell the daemon our real terminal size once
                                if !sent_resize.get() && !session.is_empty() {
                                    let rf = Frame::Resize {
                                        id: Uuid::new_v4().to_string(),
                                        client: "attach".into(),
                                        session: session.clone(),
                                        cols,
                                        rows,
                                    };
                                    send_frame(&mut stream, &rf).ok();
                                    sent_resize.set(true);
                                }
                            }
                            Frame::Update {
                                session: usess,
                                pane: upane,
                                rows_upd,
                                cursor,
                                cols,
                                rows,
                                ..
                            } => {
                                if usess == session_id {
                                    if let Some(pv) = pane_views.get_mut(&upane) {
                                        pv.apply_update(cols, rows, &rows_upd, &cursor);

                                    }
                                    if upane == active_pane {
                                        screen.cols = cols;
                                        screen.rows = rows;
                                        screen.lines.resize(rows as usize, String::new());
                                        for (idx, text) in &rows_upd {
                                            let idx = *idx as usize;
                                            if idx < screen.lines.len() {
                                                screen.lines[idx] = text.clone();
                                            }
                                        }
                                        if let Some(c) = cursor {
                                            screen.cursor = (c.x, c.y, c.visible);
                                        }
                                        if pane_views.len() > 1 {
                                            // split mode: mirror the focused pane
                                            // into the fallback screen (rows are
                                            // pane-local; top-aligned is correct)
                                            screen.cursor = cursor
                                                .map(|c| (c.x, c.y, c.visible))
                                                .unwrap_or((0, 0, false));
                                        }
                                    }
                                }
                            }
                            Frame::Meta {
                                status: Some(s), ..
                            } => {
                                eprintln!("ranch: {s}");
                            }
                            Frame::Error { message, .. } => {
                                restore();
                                drop(term);
                                die(&format!("attach failed: {message}"));
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // render
        let screen_ref = &screen;
        let prefix_now = prefix_mode.get();
        let picker_now = picker.get();
        let prompt_now = prompt.get();
        let meta_ref = &sessions_meta;
        let panes_n = panes.len();
        let sess_id_ref = &session_id;
        let views_ref = &pane_views;
        let layout_ref = &layout;
        let active_ref = &active_pane;
        let _ = term.draw(|f: &mut RFrame| {
            let area = f.area();
            let status_h = if area.height >= 2 { 1 } else { 0 };
            let term_area = Rect::new(0, 0, area.width, area.height - status_h);

            // Compute pane rects from the layout tree (50/50 splits).
            let mut rects: Vec<(String, Rect)> = Vec::new();
            if let Some(ly) = layout_ref {
                fn walk(l: &ranch_protocol::Layout, x: u16, y: u16, w: u16, h: u16, out: &mut Vec<(String, Rect)>) {
                    match l {
                        ranch_protocol::Layout::Leaf { pane } => {
                            out.push((pane.clone(), Rect::new(x, y, w.max(1), h.max(1))));
                        }
                        ranch_protocol::Layout::Split { dir, a, b, pct } => match dir {
                            1 => {
                                let lw = ((w as u32 * *pct as u32 / 100) as u16)
                                    .clamp(1, w.saturating_sub(1).max(1));
                                walk(a, x, y, lw, h, out);
                                walk(b, x + lw, y, w - lw, h, out);
                            }
                            _ => {
                                let th = ((h as u32 * *pct as u32 / 100) as u16)
                                    .clamp(1, h.saturating_sub(1).max(1));
                                walk(a, x, y, w, th, out);
                                walk(b, x, y + th, w, h - th, out);
                            }
                        },
                    }
                }
                walk(ly, 0, 0, term_area.width, term_area.height, &mut rects);
            }

            let draw_pane = |f: &mut RFrame, pane_id: &str, r: Rect, focused: bool| {
                let pv = views_ref.get(pane_id);
                let (lines_src, cx, cy, vis): (&Vec<String>, u16, u16, bool) = match pv {
                    Some(pv) => {
                        let (x, y, v) = pv.cursor.unwrap_or((0, 0, false));
                        (&pv.lines, x, y, v)
                    }
                    None => (&screen_ref.lines, screen_ref.cursor.0, screen_ref.cursor.1, false),
                };
                let mut li: Vec<Line> = Vec::with_capacity(r.height as usize);
                for row in 0..r.height as usize {
                    let line = lines_src.get(row).cloned().unwrap_or_default();
                    // parse SGR runs into styled spans; rows are one grid
                    // row each so the parse is row-local
                    use ansi_to_tui::IntoText as _;
                    let spans: Vec<Span> = match line.as_bytes().into_text() {
                        Ok(t) => t
                            .lines
                            .into_iter()
                            .next()
                            .map(|l| l.spans)
                            .unwrap_or_default(),
                        Err(_) => vec![Span::raw(line.clone())],
                    };
                    // clip spans to the rect width by cell count, applying
                    // the reversed cursor cell on the focused pane
                    let mut out: Vec<Span> = Vec::new();
                    let mut col = 0usize;
                    let cursor_cell = focused && vis && (row as u16) == cy;
                    let mut cursor_done = !cursor_cell;
                    for span in spans {
                        if col >= r.width as usize {
                            break;
                        }
                        let chars: Vec<char> = span.content.chars().collect();
                        if chars.is_empty() {
                            continue;
                        }
                        for (i, ch) in chars.iter().enumerate() {
                            if col >= r.width as usize {
                                break;
                            }
                            let is_cursor_cell = !cursor_done && col == cx as usize;
                            if is_cursor_cell {
                                out.push(Span::styled(
                                    ch.to_string(),
                                    span.style.add_modifier(Modifier::REVERSED),
                                ));
                                cursor_done = true;
                            } else {
                                out.push(Span::styled(ch.to_string(), span.style));
                            }
                            col += 1;
                            let _ = i;
                        }
                    }
                    // cursor at/past the line end: reversed blank cell
                    if cursor_cell && !cursor_done {
                        out.push(Span::styled(
                            " ".to_string(),
                            Style::default().add_modifier(Modifier::REVERSED),
                        ));
                        cursor_done = true;
                    }
                    // pad to full width so the pane background is uniform
                    while col < r.width as usize {
                        out.push(Span::raw(" ".to_string()));
                        col += 1;
                    }
                    li.push(Line::from(out));
                }
                f.render_widget(ratatui::widgets::Paragraph::new(li), r);
                // focused pane gets a border; unfocused panes a dim one
                let border_style = if focused {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                };
                let _ = border_style;
            };

            if rects.len() <= 1 {
                // single pane: full-area render (no borders, matches old behavior)
                let pid = rects.first().map(|(p, _)| p.clone()).unwrap_or_default();
                let focused = pid == *active_ref || pid.is_empty();
                draw_pane(f, &pid, term_area, focused);
            } else {
                // multi-pane: 1-cell gutters around each rect, focused pane bordered
                for (pid, r) in &rects {
                    let focused = pid == active_ref;
                    let inner = Rect::new(
                        r.x + 1,
                        r.y,
                        r.width.saturating_sub(2).max(1),
                        r.height,
                    );
                    draw_pane(f, pid, inner, focused);
                    // left/right gutter bars: bright for focused
                    let bar_style = if focused {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default().add_modifier(Modifier::DIM)
                    };
                    if r.width >= 2 {
                        f.render_widget(
                            ratatui::widgets::Paragraph::new(Span::styled("│", bar_style)),
                            Rect::new(r.x, r.y, 1, r.height),
                        );
                    }
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(Span::styled(" ", bar_style)),
                        Rect::new(r.x + r.width.saturating_sub(1), r.y, 1, r.height),
                    );
                }
            }

            // tmux-style green status bar on the last row
            if area.height >= 2 {
                let nowix = meta_ref
                    .iter()
                    .find(|s| s.id == *sess_id_ref)
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| {
                        if sess_id_ref.is_empty() { "(attaching…)".into() } else { sess_id_ref.clone() }
                    });
                let pfx = if prefix_now { "[prefix]" } else { "" };
                let status = format!(" ranch  {nowix}  {panes_n} pane{} {} ",
                    if panes_n == 1 { "" } else { "s" }, pfx);
                let bar_style = Style::default().add_modifier(Modifier::REVERSED);
                let bar = Line::from(Span::styled(format!("{:^width$}", status, width = area.width as usize), bar_style));
                f.render_widget(Paragraph::new(vec![bar]),
                    Rect::new(0, area.height - 1, area.width, 1));
            }

            // session picker overlay (prefix-s)
            if picker_now && !meta_ref.is_empty() {
                let h = (meta_ref.len() as u16 + 2).min(area.height.saturating_sub(2));
                let w = 50.min(area.width);
                let x = area.width.saturating_sub(w) / 2;
                let y = area.height.saturating_sub(h) / 2;
                let mut items: Vec<Line> = vec![
                    Line::from(Span::styled(" switch session (enter: select, esc: close)",
                        Style::default().add_modifier(Modifier::REVERSED)))];
                for (i, s2) in meta_ref.iter().enumerate() {
                    let marker = if s2.id == *sess_id_ref { "* " } else { "  " };
                    let sel = i == 0;
                    let style = if sel { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
                    items.push(Line::from(Span::styled(format!("{marker}{:<20} {} pane(s)", s2.name, s2.panes.len()), style)));
                }
                f.render_widget(ratatui::widgets::Block::bordered().title("sessions"),
                    Rect::new(x, y, w, h));
                f.render_widget(Paragraph::new(items), Rect::new(x + 1, y + 1, w.saturating_sub(2), h.saturating_sub(2)));
            }

            // prompt line (rename / command)
            if let Some(kind) = prompt_now {
                let label = match kind { Prompt::Rename => "rename session: ", Prompt::Command => ": " };
                let pl = Line::from(Span::styled(
                    format!("{label}{}\u{2588}", prompt_input.clone()),
                    Style::default().add_modifier(Modifier::REVERSED)));
                f.render_widget(Paragraph::new(vec![pl]),
                    Rect::new(0, area.height.saturating_sub(2), area.width, 1));
            }
        });

        // events
        if poll(Duration::from_millis(20)).unwrap_or(false) {
            if let Ok(event) = read_event() {
                match event {
                    CEvent::Resize(c, r) => {
                        let f = Frame::Resize {
                            id: Uuid::new_v4().to_string(),
                            client: "attach".into(),
                            session: session_id.clone(),
                            cols: c,
                            rows: r,
                        };
                        send_frame(&mut stream, &f).ok();
                    }
                    CEvent::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        // tmux-style prefix handling: Ctrl-B opens the
                        // command state; the next key is a command (or a
                        // second Ctrl-B passes the prefix through).
                        if !prefix_mode.get() {
                            if key.code == KeyCode::Char('b')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                prefix_mode.set(true);
                                continue;
                            }
                        } else {
                            prefix_mode.set(false);
                            match key.code {
                                // Ctrl-B Ctrl-B → literal Ctrl-B to the PTY
                                KeyCode::Char('b')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    let f = Frame::Input {
                                        id: Uuid::new_v4().to_string(),
                                        client: "attach".into(),
                                        session: session_id.clone(),
                                        pane: active_pane.clone(),
                                        data: ranch_protocol::b64_encode(b"\x02"),
                                    };
                                    send_frame(&mut stream, &f).ok();
                                    continue;
                                }
                                // d → detach
                                KeyCode::Char('d') => break,
                                // n / p → next / previous session (client order)
                                KeyCode::Char('n') | KeyCode::Char('p') => {
                                    let list = session_list.borrow().clone();
                                    if list.len() > 1 {
                                        let cur = list.iter().position(|s| *s == session_id);
                                        let going_next = key.code == KeyCode::Char('n');
                                        let next = match cur {
                                            Some(i) if going_next => (i + 1) % list.len(),
                                            Some(i) => (i + list.len() - 1) % list.len(),
                                            None => 0,
                                        };
                                        let target = list[next].clone();
                                        // re-attach to the other session
                                        let rf = Frame::Attach {
                                            id: Uuid::new_v4().to_string(),
                                            client: "attach".into(),
                                            session: target,
                                            pane: None,
                                        };
                                        send_frame(&mut stream, &rf).ok();
                                        screen.reset_blank();
                                        sent_resize.set(false);
                                    }
                                    continue;
                                }
                                // o / l → next pane in session
                                KeyCode::Char('o') | KeyCode::Char('l') => {
                                    if panes.len() > 1 {
                                        let idx = panes
                                            .iter()
                                            .position(|p| p == &active_pane)
                                            .map_or(0, |i| (i + 1) % panes.len());
                                        let next = panes[idx].clone();
                                        let f = Frame::SessionsSelect {
                                            session: session_id.clone(),
                                            pane: next,
                                        };
                                        send_frame(&mut stream, &f).ok();
                                    }
                                    continue;
                                }
                                // c → new session (named by daemon), auto-attach
                                KeyCode::Char('c') => {
                                    let f = Frame::SessionsCreate {
                                        req_id: Uuid::new_v4().to_string(),
                                        name: None,
                                    };
                                    send_frame(&mut stream, &f).ok();
                                    continue;
                                }
                                // & → kill current session
                                KeyCode::Char('&') | KeyCode::Char('k') => {
                                    let f = Frame::SessionsKill {
                                        session: session_id.clone(),
                                    };
                                    send_frame(&mut stream, &f).ok();
                                    screen.reset_blank();
                                    continue;
                                }
                                // , → rename (reads a name from the prompt line)
                                KeyCode::Char(',') => {
                                    prompt.set(Some(Prompt::Rename));
                                    prompt_input.clear();
                                    continue;
                                }
                                // % / " → split pane
                                KeyCode::Char('%') | KeyCode::Char('"') => {
                                    let f = Frame::PaneSplit {
                                        req_id: Uuid::new_v4().to_string(),
                                        session: session_id.clone(),
                                        pane: active_pane.clone(),
                                        dir: if key.code == KeyCode::Char('%') { 1 } else { 0 },
                                    };
                                    send_frame(&mut stream, &f).ok();
                                    continue;
                                }
                                // s → session picker overlay
                                KeyCode::Char('s') => {
                                    picker.set(!picker.get());
                                    continue;
                                }
                                // Ctrl+arrows → resize the focused pane's split
                                KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                                    if key.modifiers.contains(KeyModifiers::CONTROL)
                                        && !active_pane.is_empty() =>
                                {
                                    let (dir, delta) = match key.code {
                                        KeyCode::Left => (1u8, -2i16),
                                        KeyCode::Right => (1, 2),
                                        KeyCode::Up => (0, -2),
                                        _ => (0, 2),
                                    };
                                    let f = Frame::PaneResize {
                                        session: session_id.clone(),
                                        pane: active_pane.clone(),
                                        dir,
                                        delta,
                                    };
                                    send_frame(&mut stream, &f).ok();
                                    continue;
                                }
                                // arrows → move focus between panes (tmux-style)
                                KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                                    if !active_pane.is_empty() =>
                                {
                                    let dir = match key.code {
                                        KeyCode::Up => 0u8,
                                        KeyCode::Down => 1,
                                        KeyCode::Left => 2,
                                        _ => 3,
                                    };
                                    if let Some(ly) = &layout {
                                        let mut rects: Vec<(String, ratatui::layout::Rect)> =
                                            vec![];
                                        fn walk2(
                                            l: &ranch_protocol::Layout,
                                            x: u16, y: u16, w: u16, h: u16,
                                            out: &mut Vec<(String, ratatui::layout::Rect)>,
                                        ) {
                                            match l {
                                                ranch_protocol::Layout::Leaf { pane } => out
                                                    .push((pane.clone(), ratatui::layout::Rect::new(x, y, w.max(1), h.max(1)))),
                                                ranch_protocol::Layout::Split { dir, a, b, pct } => match dir {
                                                    1 => {
                                                        let lw = ((w as u32 * *pct as u32 / 100) as u16)
                                                            .clamp(1, w.saturating_sub(1).max(1));
                                                        walk2(a, x, y, lw, h, out);
                                                        walk2(b, x + lw, y, w - lw, h, out);
                                                    }
                                                    _ => {
                                                        let th = ((h as u32 * *pct as u32 / 100) as u16)
                                                            .clamp(1, h.saturating_sub(1).max(1));
                                                        walk2(a, x, y, w, th, out);
                                                        walk2(b, x, y + th, w, h - th, out);
                                                    }
                                                },
                                            }
                                        }
                                        walk2(ly, 0, 0, cols, rows.saturating_sub(1), &mut rects);
                                        if let Some(next) = neighbor_pane(&rects, &active_pane, dir) {
                                            let f = Frame::SessionsSelect {
                                                session: session_id.clone(),
                                                pane: next,
                                            };
                                            send_frame(&mut stream, &f).ok();
                                        }
                                    }
                                    continue;
                                }
                                // { / } → swap focused pane with previous / next
                                // pane in layout order (tmux swap-pane semantics)
                                KeyCode::Char('{') | KeyCode::Char('}') => {
                                    if let Some(ly) = &layout {
                                        let order = ranch_protocol::leaf_order(ly);
                                        if order.len() > 1 {
                                            if let Some(cur) =
                                                order.iter().position(|p| *p == active_pane)
                                            {
                                                let other = if key.code == KeyCode::Char('{') {
                                                    order[(cur + order.len() - 1) % order.len()]
                                                        .clone()
                                                } else {
                                                    order[(cur + 1) % order.len()].clone()
                                                };
                                                let f = Frame::PaneSwap {
                                                    session: session_id.clone(),
                                                    a: active_pane.clone(),
                                                    b: other,
                                                };
                                                send_frame(&mut stream, &f).ok();
                                            }
                                        }
                                    }
                                    continue;
                                }
                                // x → kill current pane
                                KeyCode::Char('x') => {
                                    let f = Frame::PaneKill {
                                        session: session_id.clone(),
                                        pane: active_pane.clone(),
                                    };
                                    send_frame(&mut stream, &f).ok();
                                    continue;
                                }
                                // q → show pane numbers briefly (MVP: status flash)
                                KeyCode::Char('q') => continue,
                                // : → command prompt (rename etc.)
                                KeyCode::Char(':') => {
                                    prompt.set(Some(Prompt::Command));
                                    prompt_input.clear();
                                    continue;
                                }
                                // prefix + any other key: pass the prefix
                                // through to the PTY as Ctrl-B
                                _ => {
                                    let f = Frame::Input {
                                        id: Uuid::new_v4().to_string(),
                                        client: "attach".into(),
                                        session: session_id.clone(),
                                        pane: active_pane.clone(),
                                        data: ranch_protocol::b64_encode(b"\x02"),
                                    };
                                    send_frame(&mut stream, &f).ok();
                                    // fall through to also send this key
                                }
                            }
                        }
                        // rename/command prompt captures printable keys
                        if prompt.get().is_some() {
                            match key.code {
                                KeyCode::Esc => {
                                    prompt.set(None);
                                    prompt_input.clear();
                                }
                                KeyCode::Enter => {
                                    if let Some(kind) = prompt.take() {
                                        match kind {
                                            Prompt::Rename => {
                                                if !prompt_input.is_empty() {
                                                    let f = Frame::SessionsRename {
                                                        session: session_id.clone(),
                                                        name: prompt_input.clone(),
                                                    };
                                                    send_frame(&mut stream, &f).ok();
                                                }
                                            }
                                            Prompt::Command => {
                                                // minimal: :kill, :detach
                                                match prompt_input.trim() {
                                                    "kill" | "kill-session" => {
                                                        let f = Frame::SessionsKill {
                                                            session: session_id.clone(),
                                                        };
                                                        send_frame(&mut stream, &f).ok();
                                                        screen.reset_blank();
                                                    }
                                                    "detach" | "d" => {
                                                        prompt_input.clear();
                                                        break;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }
                                    prompt_input.clear();
                                }
                                KeyCode::Backspace => {
                                    prompt_input.pop();
                                }
                                KeyCode::Char(c) => prompt_input.push(c),
                                _ => {}
                            }
                            continue;
                        }
                        if let Some(bytes) = key_to_bytes(&key) {
                            let f = Frame::Input {
                                id: Uuid::new_v4().to_string(),
                                client: "attach".into(),
                                session: session_id.clone(),
                                pane: active_pane.clone(),
                                data: ranch_protocol::b64_encode(&bytes),
                            };
                            send_frame(&mut stream, &f).ok();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // detach
    let detach = Frame::Detach {
        id: Uuid::new_v4().to_string(),
        client: "attach".into(),
    };
    send_frame(&mut stream, &detach).ok();

    restore();
    drop(term);
    println!("detached");
}

// ---------- main ----------

// ---------- cloud commands ----------

/// Supabase config the client learns once (from the project owner) and
/// stores at ~/.config/ranch/config.json. Not secret: the anon key and
/// project URL are public by design.
struct CloudCfg {
    supabase_url: String,
    anon_key: String,
}

impl CloudCfg {
    fn path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(".config/ranch/config.json")
    }
    /// Baked-in defaults (the Ranch project). Overridable via
    /// `ranch config` (writes config.json) or RANCH_SUPABASE_URL /
    /// RANCH_ANON_KEY env vars.
    fn default_cfg() -> CloudCfg {
        CloudCfg {
            supabase_url: option_env!("RANCH_DEFAULT_SUPABASE_URL")
                .unwrap_or("https://prqfseydoxyingbkmiic.supabase.co")
                .to_string(),
            anon_key: option_env!("RANCH_DEFAULT_ANON_KEY").unwrap_or(
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InBycWZzZXlkb3h5aW5nYmttaWljIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODg4NDk2NzQsImV4cCI6MjEwNDQyNTY3NH0.lGEKMCE_dWvIDrkXjdXz3KTZtC7Nbd9EtBDSmRYJ3mU",
            ).to_string(),
        }
    }
    fn load() -> CloudCfg {
        // env override > config.json > baked-in default
        if let (Ok(u), Ok(k)) = (
            std::env::var("RANCH_SUPABASE_URL"),
            std::env::var("RANCH_ANON_KEY"),
        ) {
            return CloudCfg {
                supabase_url: u.trim_end_matches('/').to_string(),
                anon_key: k,
            };
        }
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let (Some(u), Some(k)) = (
                    v["supabase_url"].as_str(),
                    v["anon_key"].as_str(),
                ) {
                    return CloudCfg {
                        supabase_url: u.to_string(),
                        anon_key: k.to_string(),
                    };
                }
            }
        }
        Self::default_cfg()
    }
    fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, serde_json::json!({
            "supabase_url": self.supabase_url,
            "anon_key": self.anon_key,
        }).to_string()).map_err(|e| e.to_string())
    }
}

/// Auth session for the human user (owner). Stored 0600.
/// refresh token is long-lived; access token is refreshed on demand.
#[derive(serde::Deserialize)]
struct UserSession {
    access_token: String,
    refresh_token: String,
    expires_at: u64, // unix seconds
    user_id: String,
    email: String,
}

impl UserSession {
    fn path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(".config/ranch/user.json")
    }
    fn load() -> Option<UserSession> {
        let text = std::fs::read_to_string(Self::path()).ok()?;
        serde_json::from_str(&text).ok()
    }
    fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, serde_json::json!({
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "expires_at": self.expires_at,
            "user_id": self.user_id,
            "email": self.email,
        }).to_string()).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).ok();
        Ok(())
    }
    fn valid(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        now + 60 < self.expires_at
    }
}

fn http_json(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Result<(u16, serde_json::Value), String> {
    // ureq 3 types differ per body-ness, so dispatch per method with a
    // small macro to apply headers uniformly.
    macro_rules! hdrs {
        ($r:expr) => {{
            let mut r = $r;
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            r
        }};
    }
    let resp = match method.to_ascii_uppercase().as_str() {
        "GET" => hdrs!(ureq::get(url)).call(),
        "DELETE" => hdrs!(ureq::delete(url)).call(),
        "POST" => hdrs!(ureq::post(url)).send_json(body.unwrap_or(serde_json::Value::Null)),
        "PATCH" => hdrs!(ureq::patch(url)).send_json(body.unwrap_or(serde_json::Value::Null)),
        _ => return Err(format!("unsupported method {method}")),
    };
    let mut resp = resp.map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status().as_u16();
    let mut text = String::new();
    resp.body_mut().as_reader().read_to_string(&mut text).map_err(|e| e.to_string())?;
    let v = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    Ok((status, v))
}

/// b64url decode for JWT payload inspection.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let val = |b: u8| -> Option<u32> { A.iter().position(|&a| a == b).map(|p| p as u32) };
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for ch in bytes.chunks(4) {
        let v0 = val(*ch.first()?)?;
        let v1 = val(*ch.get(1)?)?;
        let v2 = ch.get(2).copied().and_then(val).unwrap_or(0);
        let v3 = ch.get(3).copied().and_then(val).unwrap_or(0);
        let n = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        out.push((n >> 16) as u8);
        if ch.len() > 2 { out.push((n >> 8) as u8); }
        if ch.len() > 3 { out.push(n as u8); }
    }
    Some(out)
}

fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let mid = jwt.split('.').nth(1)?;
    let pad = (4 - mid.len() % 4) % 4;
    let decoded = b64url_decode(&format!("{mid}{}", "=".repeat(pad)))?;
    serde_json::from_slice(&decoded).ok()
}

/// Interactive login: opens the browser for Google OAuth via Supabase,
/// then polls for the session (token-binding via the supabase-js-ish
/// implicit flow is fragile — we use the PKCE-style magic: start an
/// OAuth flow against /auth/v1/authorize and read the code from the
/// redirect. Simplest robust path for a CLI: device-less browser flow
/// with localhost callback listener.
fn cmd_login(email: Option<String>) {
    let cfg = CloudCfg::load();
    let session = match email {
        // email/password fallback: works before Google OAuth is configured
        // and on headless machines.
        Some(e) => {
            let password = rpassword::prompt_password("password: ").unwrap_or_else(|e| die(&format!("login: {e}")));
            let email = e;
            let (status, body) = match http_json(
                "POST",
                &format!("{}/auth/v1/token?grant_type=password", cfg.supabase_url),
                &[("apikey", &cfg.anon_key), ("Content-Type", "application/json")],
                Some(serde_json::json!({ "email": email, "password": password })),
            ) {
                Ok(r) => r,
                Err(err) => die(&format!("login: {err}")),
            };
            if status != 200 {
                die(&format!("login failed ({status}): {body}"));
            }
            let claims = jwt_claims(body["access_token"].as_str().unwrap_or_default());
            UserSession {
                access_token: body["access_token"].as_str().unwrap_or_default().to_string(),
                refresh_token: body["refresh_token"].as_str().unwrap_or_default().to_string(),
                expires_at: body["expires_at"].as_u64().unwrap_or(0),
                user_id: claims.as_ref().and_then(|c| c["sub"].as_str()).unwrap_or_default().to_string(),
                email: claims.as_ref().and_then(|c| c["email"].as_str()).unwrap_or_default().to_string(),
            }
        }
        // default: Google OAuth via the browser (PKCE + localhost callback)
        None => oauth_login(&cfg),
    };
    session.save().unwrap_or_else(|e| die(&format!("login: save session: {e}")));
    println!("logged in as {} ({})", session.email, session.user_id);
}

/// Google OAuth (PKCE) via the system browser + localhost callback.
fn oauth_login(cfg: &CloudCfg) -> UserSession {
    let verifier = gen_pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let redirect_port = 8737u16;
    let redirect = format!("http://localhost:{redirect_port}/callback");

    println!("opening browser for Google sign-in…");
    let url = format!(
        "{}/auth/v1/authorize?provider=google&redirect_to={}&code_challenge={}&code_challenge_method=S256",
        cfg.supabase_url,
        urlencode(&redirect),
        challenge
    );
    open_browser(&url);

    let code = match listen_for_code(redirect_port, 300) {
        Some(c) => c,
        None => die("login: timed out waiting for browser callback"),
    };

    let (status, body) = match http_json(
        "POST",
        &format!("{}/auth/v1/token?grant_type=pkce", cfg.supabase_url),
        &[("apikey", &cfg.anon_key), ("Content-Type", "application/json")],
        Some(serde_json::json!({
            "auth_code": code,
            "code_verifier": verifier,
        })),
    ) {
        Ok(r) => r,
        Err(e) => die(&format!("login: token exchange: {e}")),
    };
    if status != 200 {
        die(&format!("login: token exchange failed ({status}): {body}"));
    }
    let claims = jwt_claims(body["access_token"].as_str().unwrap_or_default());
    UserSession {
        access_token: body["access_token"].as_str().unwrap_or_default().to_string(),
        refresh_token: body["refresh_token"].as_str().unwrap_or_default().to_string(),
        expires_at: body["expires_at"].as_u64().unwrap_or(0),
        user_id: claims.as_ref().and_then(|c| c["sub"].as_str()).unwrap_or_default().to_string(),
        email: claims.as_ref().and_then(|c| c["email"].as_str()).unwrap_or_default().to_string(),
    }
}

/// Refresh access token using the stored refresh token; returns fresh session.
fn ensure_session(cfg: &CloudCfg) -> UserSession {
    let mut s = match UserSession::load() {
        Some(s) => s,
        None => die("not logged in — run `ranch login`"),
    };
    if s.valid() {
        return s;
    }
    let (status, body) = http_json(
        "POST",
        &format!("{}/auth/v1/token?grant_type=refresh_token", cfg.supabase_url),
        &[("apikey", &cfg.anon_key), ("Content-Type", "application/json")],
        Some(serde_json::json!({ "refresh_token": s.refresh_token })),
    ).unwrap_or_else(|e| die(&format!("refresh session: {e}")));
    if status != 200 {
        die(&format!("session expired ({}), run `ranch login`", status));
    }
    s.access_token = body["access_token"].as_str().unwrap_or_default().to_string();
    s.refresh_token = body["refresh_token"].as_str().unwrap_or_default().to_string();
    s.expires_at = body["expires_at"].as_u64().unwrap_or(0);
    s.save().ok();
    s
}

/// List all machines owned by the logged-in user, with sessions count.
fn cmd_machines() {
    let cfg = CloudCfg::load();
    let s = ensure_session(&cfg);
    let (status, body) = http_json(
        "GET",
        &format!("{}/rest/v1/machines_info?select=id,name,last_seen_at&order=name", cfg.supabase_url),
        &[("apikey", &cfg.anon_key), ("Authorization", &format!("Bearer {}", s.access_token))],
        None,
    ).unwrap_or_else(|e| die(&format!("machines: {e}")));
    if status != 200 {
        die(&format!("machines: {body}"));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let list = body.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no machines registered — run `ranch register <name>` on a machine");
        return;
    }
    for m in &list {
        let name = m["name"].as_str().unwrap_or("?");
        let id = m["id"].as_str().unwrap_or("?");
        let last = m["last_seen_at"].as_str().unwrap_or("");
        let online = last.contains("T") && {
            // cheap staleness check: parse ISO ts to unix secs
            epoch_from_iso(last).map(|t| now.saturating_sub(t) < 90).unwrap_or(false)
        };
        println!("  {}{}  {}  {}", if online { "●" } else { "○" }, name, id, last);
    }
}

/// List sessions across all machines (or one), from the mirror.
fn cmd_cloud_sessions(machine: Option<String>) {
    let cfg = CloudCfg::load();
    let s = ensure_session(&cfg);
    let mut url = format!(
        "{}/rest/v1/sessions?select=id,name,kind,machine_id,machines_info!inner(name)&order=name",
        cfg.supabase_url
    );
    if let Some(m) = &machine {
        url = format!(
            "{}/rest/v1/sessions?select=id,name,kind,machine_id,machines_info!inner(name)&machine_id=eq.{}&order=name",
            cfg.supabase_url, m
        );
    }
    let (status, body) = http_json("GET", &url,
        &[("apikey", &cfg.anon_key), ("Authorization", &format!("Bearer {}", s.access_token))],
        None,
    ).unwrap_or_else(|e| die(&format!("sessions: {e}")));
    if status != 200 {
        die(&format!("sessions: {body}"));
    }
    for row in body.as_array().cloned().unwrap_or_default() {
        let mname = row["machines_info"]["name"].as_str().unwrap_or("?");
        println!("  {}@{}  {}  {}", row["name"].as_str().unwrap_or("?"), mname, row["kind"].as_str().unwrap_or("shell"), row["id"].as_str().unwrap_or("?"));
    }
}

fn epoch_from_iso(ts: &str) -> Option<u64> {
    // "2026-09-08T13:46:04+00:00" → unix secs (UTC only; good enough for staleness)
    let (date, time) = ts.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let da: i64 = dp.next()?.parse().ok()?;
    let tp = time.trim_end_matches('Z').split('+').next()?.split('-').next()?.to_string();
    let mut hp = tp.split(':');
    let h: i64 = hp.next()?.parse().ok()?;
    let mi: i64 = hp.next()?.parse().ok()?;
    let se: f64 = hp.next().unwrap_or("0").parse().ok()?;
    let days = days_from_civil(y, mo, da);
    Some(((days * 86400) + h * 3600 + mi * 60 + se as i64) as u64)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn gen_pkce_verifier() -> String {
    use std::io::Read;
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut buf = [0u8; 64];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|&b| ALPHA[b as usize % ALPHA.len()] as char).collect()
}

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    b64url_encode(&digest)
}

fn b64url_encode(data: &[u8]) -> String {
    const B: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for ch in data.chunks(3) {
        let b0 = ch[0] as u32;
        let b1 = ch.get(1).copied().unwrap_or(0) as u32;
        let b2 = ch.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B[(n >> 18) as usize & 63] as char);
        out.push(B[(n >> 12) as usize & 63] as char);
        out.push(if ch.len() > 1 { B[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if ch.len() > 2 { B[n as usize & 63] as char } else { '=' });
    }
    out.trim_end_matches('=').to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn open_browser(url: &str) {
    println!("sign-in URL (browser should open; copy this if not):\n  {url}");
    if let Ok(b) = std::env::var("BROWSER") {
        if !b.is_empty() {
            let _ = std::process::Command::new(b).arg(url).status();
            return;
        }
    }
    let _ = std::process::Command::new("xdg-open").arg(url).status()
        .or_else(|_| std::process::Command::new("open").arg(url).status());
}

/// Tiny localhost HTTP listener to catch the OAuth redirect code.
fn listen_for_code(port: u16, timeout_secs: u64) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
    let start = Instant::now();
    listener.set_nonblocking(false).ok();
    loop {
        if start.elapsed() > Duration::from_secs(timeout_secs) {
            return None;
        }
        listener.set_nonblocking(true).ok();
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                // GET /callback?code=… HTTP/1.1
                let code = req.split_whitespace().nth(1)
                    .and_then(|path| path.split('?').nth(1))
                    .and_then(|q| q.split('&').find(|kv| kv.starts_with("code=")))
                    .map(|kv| kv[5..].to_string());
                let resp = if code.is_some() {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>Ranch: signed in.</h1><script>setTimeout(()=>window.close(),600)</script>"
                } else {
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h1>Ranch: missing ?code</h1>"
                };
                stream.write_all(resp.as_bytes()).ok();
                if let Some(c) = code {
                    return Some(urldecode(&c));
                }
                // browser may retry; keep listening
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => { out.push(b' '); i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `ranch config <supabase-url> <anon-key>` — one-time client bootstrap.
/// `ranch register [name]` — registers THIS machine with the logged-in
/// user's account via the register_machine RPC (no service role needed).
/// Prints the machine key once and writes 0600 daemon.toml for ranchd.
/// `ranch register [name]` — registers THIS machine with the logged-in
/// user's account via the register_machine RPC (no service role needed).
/// Prints the machine key once and writes 0600 daemon.toml for ranchd.
fn cmd_register(name: Option<String>) {
    let cfg = CloudCfg::load();
    let session = ensure_session(&cfg);

    let name = name.unwrap_or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "machine".into())
    });

    let cfg_path = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/ranch/daemon.toml"))
        .unwrap_or_else(|_| std::path::PathBuf::from("daemon.toml"));
    if cfg_path.exists() {
        die(&format!(
            "register: {} already exists — delete it first to re-register",
            cfg_path.display()
        ));
    }

    let (status, body) = http_json(
        "POST",
        &format!("{}/rest/v1/rpc/register_machine", cfg.supabase_url),
        &[
            ("apikey", cfg.anon_key.as_str()),
            ("Authorization", &format!("Bearer {}", session.access_token)),
            ("Content-Type", "application/json"),
        ],
        Some(serde_json::json!({ "p_name": name })),
    ).unwrap_or_else(|e| die(&format!("register: {e}")));
    if status != 200 {
        die(&format!("register: RPC failed ({status}): {body}"));
    }
    let row = body.as_array().and_then(|a| a.first().cloned())
        .unwrap_or_else(|| die(&format!("register: unexpected RPC response: {body}")));
    let machine_id = row["machine_id"].as_str().unwrap_or_default().to_string();
    let machine_key = row["machine_key"].as_str().unwrap_or_default().to_string();
    let email = format!("machine-{machine_id}@ranch.local");

    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let cfg_text = format!(
        "# ranch daemon config — machine credentials (chmod 600).\n\
         # Generated by `ranch register`; the key is the password of the\n\
         # machine's Supabase Auth user and gates the Realtime channel.\n\
         machine_id = \"{machine_id}\"\n\
         machine_email = \"{email}\"\n\
         machine_key = \"{machine_key}\"\n\
         supabase_url = \"{}\"\n\
         anon_key = \"{}\"\n",
        cfg.supabase_url, cfg.anon_key
    );
    std::fs::write(&cfg_path, &cfg_text)
        .unwrap_or_else(|e| die(&format!("register: write config: {e}")));
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600)).ok();

    println!("machine registered");
    println!("  name:   {name}");
    println!("  id:     {machine_id}");
    println!("  config: {}", cfg_path.display());
    println!();
    println!("  machine key (shown once, also stored in the config above):");
    println!("    {machine_key}");
    println!();
    println!("restart ranchd to connect the relay.");
}

/// `ranch config <supabase-url> <anon-key>` — one-time client bootstrap.
fn cmd_config(url: String, key: String) {
    let cfg = CloudCfg {
        supabase_url: url.trim_end_matches('/').to_string(),
        anon_key: key,
    };
    cfg.save().unwrap_or_else(|e| die(&format!("config: {e}")));
    println!("cloud config saved to {}", CloudCfg::path().display());
}

fn libc_isatty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        // TTY → interactive session manager; otherwise print usage.
        if libc_isatty() {
            loop {
                match cmd_dashboard() {
                    Some(ref_) => cmd_attach(&ref_), // detach returns here
                    None => break,
                }
            }
            return;
        }
        eprintln!("usage: ranch <new|ls|attach|kill|rename|split|switch|register|login|machines|cloud> [args]");
        eprintln!("  (run plain `ranch` in a terminal for the interactive session manager)");
        eprintln!("  socket: {}", socket_path().display());
        std::process::exit(2);
    }
    match args[0].as_str() {
        "register" => cmd_register(args.get(1).cloned()),
        "login" => cmd_login(args.get(1).cloned()),
        "config" => match (args.get(1), args.get(2)) {
            (Some(u), Some(k)) => cmd_config(u.clone(), k.clone()),
            _ => die("usage: ranch config <supabase-url> <anon-key>"),
        },
        "machines" => cmd_machines(),
        "cloud" => cmd_cloud_sessions(args.get(1).cloned()),
        "new" => cmd_new(args.get(1).cloned()),
        "ls" | "list" => cmd_ls(),
        "attach" => match args.get(1) {
            Some(r) => cmd_attach(r),
            None => die("usage: ranch attach <session>"),
        },
        "kill" => match args.get(1) {
            Some(r) => cmd_kill(r),
            None => die("usage: ranch kill <session>"),
        },
        "rename" => match (args.get(1), args.get(2)) {
            (Some(r), Some(n)) => cmd_rename(r, n),
            _ => die("usage: ranch rename <session> <new-name>"),
        },
        "split" => match args.get(1) {
            Some(r) => cmd_split(r),
            None => die("usage: ranch split <session>"),
        },
        "switch" => match (args.get(1), args.get(2)) {
            (Some(r), Some(p)) => cmd_switch(r, p),
            _ => die("usage: ranch switch <session> <pane>"),
        },
        other => die(&format!("unknown command {other:?}")),
    }
}
