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
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read as read_event,
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
    let mut decoder = Decoder::new();
    let mut buf = [0u8; 65536];
    let got_snapshot = std::cell::Cell::new(false);
    let sent_resize = std::cell::Cell::new(false);

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
                            Frame::Snapshot {
                                session,
                                panes: panes_snap,
                                active_pane: ap,
                                ..
                            } => {
                                if session_id.is_empty() || session_id != session {
                                    session_id = session.clone();
                                }
                                active_pane = ap;
                                panes = panes_snap.iter().map(|p| p.id.clone()).collect();
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
                                rows_upd,
                                cursor,
                                cols,
                                rows,
                                ..
                            } => {
                                screen.cols = cols;
                                screen.rows = rows;
                                screen.lines.resize(rows as usize, String::new());
                                for (idx, text) in rows_upd {
                                    let idx = idx as usize;
                                    if idx < screen.lines.len() {
                                        screen.lines[idx] = text;
                                    }
                                }
                                if let Some(c) = cursor {
                                    screen.cursor = (c.x, c.y, c.visible);
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
        let _ = term.draw(move |f: &mut RFrame| {
            let area = f.area();
            let (cx, cy, vis) = screen_ref.cursor;
            let lines: Vec<Line> = screen_ref
                .lines
                .iter()
                .take(area.height as usize)
                .enumerate()
                .map(|(i, line)| {
                    if vis && (i as u16) == cy {
                        // cursor x is a column (char index), not a byte offset
                        let start = line
                            .char_indices()
                            .nth(cx as usize)
                            .map(|(b, _)| b)
                            .unwrap_or(line.len());
                        let end = line
                            .char_indices()
                            .nth(cx as usize + 1)
                            .map(|(b, _)| b)
                            .unwrap_or(line.len());
                        let mut spans: Vec<Span> = Vec::new();
                        spans.push(Span::raw(line[..start].to_string()));
                        if end > start {
                            spans.push(Span::styled(
                                line[start..end].to_string(),
                                Style::default().add_modifier(Modifier::REVERSED),
                            ));
                            spans.push(Span::raw(line[end..].to_string()));
                        } else {
                            spans.push(Span::styled(
                                " ".to_string(),
                                Style::default().add_modifier(Modifier::REVERSED),
                            ));
                        }
                        Line::from(spans)
                    } else {
                        Line::raw(line.clone())
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(lines), Rect::new(0, 0, area.width, area.height));
        });

        // events
        if poll(Duration::from_millis(20)).unwrap_or(false) {
            if let Ok(event) = read_event() {
                match event {
                    Event::Resize(c, r) => {
                        let f = Frame::Resize {
                            id: Uuid::new_v4().to_string(),
                            client: "attach".into(),
                            session: session_id.clone(),
                            cols: c,
                            rows: r,
                        };
                        send_frame(&mut stream, &f).ok();
                    }
                    Event::Key(key) => {
                        if key.kind == KeyEventKind::Press
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            break; // detach
                        }
                        // Alt-p: cycle to the next pane
                        if key.kind == KeyEventKind::Press
                            && key.code == KeyCode::Char('p')
                            && key.modifiers.contains(KeyModifiers::ALT)
                        {
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: ranch <new|ls|attach|kill|rename|split|switch> [args]");
        eprintln!("  socket: {}", socket_path().display());
        std::process::exit(2);
    }
    match args[0].as_str() {
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
