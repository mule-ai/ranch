//! Ranch relay client — bridges ranchd to the Supabase Realtime channel
//! for this machine (SPEC §4.3).
//!
//! One background thread owns the WebSocket. Two pipes connect it to the
//! daemon's poll loop, so the relay is "just another client" with an fd:
//!
//! ```text
//!   remote frames:  WS ──▶ relay thread ──▶ to_daemon pipe ──▶ main loop
//!   daemon frames:  main loop ──▶ to_relay pipe ──▶ relay thread ──▶ WS
//! ```
//!
//! Every ranch frame travels as one Realtime broadcast message on the
//! private channel `realtime:machines:<machine_id>`:
//! `{topic, event:"broadcast", payload:{event:"frame", payload:<frame>}}`.
//!
//! The thread also does the cloud bookkeeping that is meaningless without
//! the relay: PostgREST heartbeat (`machines.last_seen_at`), and the
//! `sessions` registry mirror (upsert on create/rename, delete on kill),
//! both authenticated with the machine user's JWT.

use std::io::Write;
use std::os::fd::RawFd;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::clog;

// ---------- config ----------

pub struct RelayConfig {
    pub machine_id: String,
    pub machine_email: String,
    pub machine_key: String,
    pub supabase_url: String,
    pub anon_key: String,
}

/// Load `~/.config/ranch/daemon.toml` (0600, written by `ranch register`).
/// Returns None when the file is absent — relay disabled, local-only mode.
pub fn load_config() -> Option<RelayConfig> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::PathBuf::from(home).join(".config/ranch/daemon.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    Some(parse_config(&text))
}

/// Minimal parser for the flat `key = "value"` daemon.toml we write.
fn parse_config(text: &str) -> RelayConfig {
    let get = |key: &str| -> String {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(key) {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    let v = rest.trim().trim_matches('"');
                    return v.to_string();
                }
            }
        }
        String::new()
    };
    RelayConfig {
        machine_id: get("machine_id"),
        machine_email: get("machine_email"),
        machine_key: get("machine_key"),
        supabase_url: get("supabase_url").trim_end_matches('/').to_string(),
        anon_key: get("anon_key"),
    }
}

// ---------- mirror ops (main loop -> relay thread) ----------

#[derive(Debug)]
pub enum RelayOut {
    /// Upsert a session row (create or rename).
    UpsertSession { id: String, name: String, kind: String },
    /// Remove a session row (kill).
    DeleteSession { id: String },
}

// ---------- thread plumbing ----------

/// Create both pipes (O_CLOEXEC) used to talk to the relay thread.
/// Returns ((main-loop read end, main-loop write end), (thread read end, thread write end)).
pub fn make_pipes() -> Result<((RawFd, std::fs::File), (RawFd, std::fs::File)), String> {
    fn pipe() -> Result<(RawFd, std::fs::File), String> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
        }
        let r = fds[0];
        let w = ffrom(fds[1]);
        Ok((r, w))
    }
    let (daemon_r, daemon_w) = pipe()?; // remote frames -> main loop
    let (relay_r, relay_w) = pipe()?; // main loop frames -> thread -> WS
    Ok(((daemon_r, daemon_w), (relay_r, relay_w)))
}

// SAFETY: fds come from pipe2 and are not aliased elsewhere.
fn ffrom(fd: RawFd) -> std::fs::File {
    use std::os::fd::FromRawFd;
    unsafe { std::fs::File::from_raw_fd(fd) }
}

/// Spawn the relay thread. Owns the WS connection for the life of the daemon.
pub fn spawn(
    cfg: RelayConfig,
    to_daemon_w: std::fs::File,
    to_relay_r: RawFd,
    mirror_rx: mpsc::Receiver<RelayOut>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("ranch-relay".into())
        .spawn(move || run(cfg, to_daemon_w, to_relay_r, mirror_rx))
        .expect("spawn relay thread")
}

fn run(
    cfg: RelayConfig,
    mut to_daemon_w: std::fs::File,
    to_relay_r: RawFd,
    mirror_rx: mpsc::Receiver<RelayOut>,
) {
    let topic = format!("realtime:machines:{}", cfg.machine_id);
    let mut backoff = 1u64;
    loop {
        let jwt = match login(&cfg) {
            Ok(j) => j,
            Err(e) => {
                clog(&format!("relay: login failed: {e} — retrying in {backoff}s"));
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        match ws_session(&cfg, &topic, &jwt, &mut to_daemon_w, to_relay_r, &mirror_rx) {
            Ok(()) => {
                clog("relay: session ended");
                backoff = 1;
            }
            Err(e) => {
                clog(&format!("relay: session error: {e} — reconnecting in {backoff}s"));
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        }
        // drain any pending mirror ops so they aren't replayed against a
        // stale connection context
        while mirror_rx.try_recv().is_ok() {}
    }
}

// ---------- supabase REST ----------

fn now_iso() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, mo, da, h, mi, s) = crate::civil_from_unix(secs as i64);
    format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn login(cfg: &RelayConfig) -> Result<String, String> {
    let url = format!(
        "{}/auth/v1/token?grant_type=password",
        cfg.supabase_url
    );
    let mut resp = ureq::post(&url)
        .header("apikey", &cfg.anon_key)
        .send_json(serde_json::json!({
            "email": cfg.machine_email,
            "password": cfg.machine_key,
        }))
        .map_err(|e| format!("http: {e}"))?;
    let body: Value = resp.body_mut().read_json().map_err(|e| format!("body: {e}"))?;
    match body.get("access_token").and_then(|v| v.as_str()) {
        Some(t) => Ok(t.to_string()),
        None => Err(format!("no access_token in login response: {body}")),
    }
}

/// PATCH machines.last_seen_at (via the machines_info updatable view).
fn heartbeat(cfg: &RelayConfig, jwt: &str) -> Result<(), String> {
    let url = format!(
        "{}/rest/v1/machines_info?id=eq.{}",
        cfg.supabase_url, cfg.machine_id
    );
    ureq::patch(&url)
        .header("apikey", &cfg.anon_key)
        .header("Authorization", &format!("Bearer {jwt}"))
        .send_json(serde_json::json!({ "last_seen_at": now_iso() }))
        .map_err(|e| format!("heartbeat: {e}"))?;
    Ok(())
}

fn mirror_upsert(cfg: &RelayConfig, jwt: &str, op: &RelayOut) {
    let (url, body) = match op {
        RelayOut::UpsertSession { id, name, kind } => (
            format!("{}/rest/v1/sessions?on_conflict=id", cfg.supabase_url),
            serde_json::json!({
                "id": id, "machine_id": cfg.machine_id,
                "name": name, "kind": kind, "last_active_at": now_iso(),
            }),
        ),
        RelayOut::DeleteSession { id } => {
            let url = format!("{}/rest/v1/sessions?id=eq.{id}", cfg.supabase_url);
            match ureq::delete(&url)
                .header("apikey", &cfg.anon_key)
                .header("Authorization", &format!("Bearer {jwt}"))
                .call()
            {
                Ok(_) => clog(&format!("relay: mirrored session delete {id}")),
                Err(e) => clog(&format!("relay: mirror delete failed: {e}")),
            }
            return;
        }
    };
    match ureq::post(&url)
        .header("apikey", &cfg.anon_key)
        .header("Authorization", &format!("Bearer {jwt}"))
        .header("Prefer", "resolution=merge-duplicates")
        .send_json(body)
    {
        Ok(_) => {}
        Err(e) => clog(&format!("relay: mirror upsert failed: {e}")),
    }
}

// ---------- websocket session ----------

fn ws_session(
    cfg: &RelayConfig,
    topic: &str,
    jwt: &str,
    to_daemon_w: &mut std::fs::File,
    to_relay_r: RawFd,
    mirror_rx: &mpsc::Receiver<RelayOut>,
) -> Result<(), String> {
    use tungstenite::{Error as WsError, Message};

    let ws_url = format!(
        "{}/realtime/v1?apikey={}&vsn=1.0.0",
        cfg.supabase_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1),
        cfg.anon_key
    );
    let (mut ws, _resp) =
        tungstenite::connect(&ws_url).map_err(|e| format!("ws connect: {e}"))?;
    clog(&format!("relay: connected, joining {topic}"));

    // join the private channel; the machine JWT rides in the join payload
    let join = serde_json::json!({
        "topic": topic,
        "event": "phx_join",
        "ref": "join",
        "payload": {
            "config": { "broadcast": {}, "presence": {}, "postgres_changes": [], "private": true },
            "access_token": jwt,
        },
    });
    ws_send(&mut ws, &join.to_string())?;

    use std::os::fd::AsRawFd as _;
    let ws_fd = match ws.get_ref() {
        tungstenite::stream::MaybeTlsStream::Plain(t) => {
            set_nonblocking(t.as_raw_fd())?;
            t.as_raw_fd()
        }
        tungstenite::stream::MaybeTlsStream::Rustls(s) => {
            let fd = s.get_ref().as_raw_fd();
            set_nonblocking(fd)?;
            fd
        }
        _ => return Err("unsupported tls backend".into()),
    };
    let mut buf = [0u8; 8192];
    let mut pending = Vec::new(); // partial line buffer for to_relay reads
    let mut last_hb = instant_now();
    let mut last_seen = instant_now();
    let mut join_ok = false;

    loop {
        // poll both the socket and the daemon->relay pipe
        let mut fds = [
            libc::pollfd { fd: ws_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: to_relay_r, events: libc::POLLIN, revents: 0 },
        ];
        let timeout: i32 = 1000; // 1s tick for timers
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll: {err}"));
        }

        // --- realtime socket ---
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            loop {
                match ws.read() {
                    Ok(Message::Text(text)) => {
                        handle_ws_text(topic, &text, to_daemon_w, &mut ws, &mut join_ok)?;
                    }
                    Ok(Message::Binary(b)) => {
                        if let Ok(text) = String::from_utf8(b.to_vec()) {
                            handle_ws_text(topic, &text, to_daemon_w, &mut ws, &mut join_ok)?;
                        }
                    }
                    Ok(Message::Ping(p)) => {
                        ws.send(Message::Pong(p)).map_err(|e| format!("pong: {e}"))?;
                    }
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Close(f)) => {
                        return Err(format!("server closed: {f:?}"));
                    }
                    Ok(_) => {}
                    Err(WsError::ConnectionClosed) => break,
                    Err(WsError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        break;
                    }
                    Err(e) => return Err(format!("ws read: {e}")),
                }
                if !join_ok {
                    // keep draining until the join reply is seen
                }
            }
        }

        // --- daemon frames to broadcast ---
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let r = unsafe {
                libc::read(to_relay_r, buf.as_mut_ptr() as *mut _, buf.len())
            };
            if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(format!("pipe read: {err}"));
                }
            } else if r == 0 {
                return Err("daemon pipe closed".into());
            } else {
                pending.extend_from_slice(&buf[..r as usize]);
                while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=nl).collect();
                    let line = std::str::from_utf8(&line[..line.len() - 1]).unwrap_or("");
                    if line.is_empty() {
                        continue;
                    }
                    clog(&format!("relay: broadcasting frame ({} bytes)", line.len()));
                    // each line is a complete ranch frame; wrap as broadcast
                    match serde_json::from_str::<Value>(line) {
                        Ok(frame) => {
                            let msg = serde_json::json!({
                                "topic": topic,
                                "event": "broadcast",
                                "ref": next_ref(),
                                "payload": { "event": "frame", "payload": frame },
                            });
                            ws_send(&mut ws, &msg.to_string())?;
                        }
                        Err(_) => clog("relay: dropping unparseable daemon frame"),
                    }
                }
            }
        }

        // --- timers ---
        if instant_now() - last_hb > 25.0 {
            let hb = serde_json::json!({
                "topic": "phoenix", "event": "phx_heartbeat", "payload": {}, "ref": next_ref(),
            });
            ws_send(&mut ws, &hb.to_string())?;
            last_hb = instant_now();
        }
        if instant_now() - last_seen > 30.0 {
            if let Err(e) = heartbeat(cfg, jwt) {
                clog(&format!("relay: {e}"));
            }
            last_seen = instant_now();
        }

        // --- session mirror ops ---
        while let Ok(op) = mirror_rx.try_recv() {
            mirror_upsert(cfg, jwt, &op);
        }
    }
}

fn handle_ws_text(
    topic: &str,
    text: &str,
    to_daemon_w: &mut std::fs::File,
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    join_ok: &mut bool,
) -> Result<(), String> {
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let event = msg.get("event").and_then(|v| v.as_str()).unwrap_or("");
    let msg_topic = msg.get("topic").and_then(|v| v.as_str()).unwrap_or("");
    let payload = msg.get("payload").cloned().unwrap_or(Value::Null);
    match event {
        "phx_reply" => {
            let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if msg_topic == topic {
                if status == "ok" {
                    if !*join_ok {
                        clog("relay: channel joined");
                        *join_ok = true;
                    }
                } else {
                    let reason = payload
                        .pointer("/response/reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    return Err(format!("join rejected: {reason}"));
                }
            }
        }
        "phx_heartbeat" => {
            // echo the heartbeat back on the phoenix topic with same ref
            let reply = serde_json::json!({
                "topic": "phoenix", "event": "phx_heartbeat",
                "payload": {}, "ref": msg.get("ref").cloned().unwrap_or(Value::Null),
            });
            ws_send(ws, &reply.to_string())?;
        }
        "broadcast" => {
            // remote client frame: payload = {event:"frame", payload:<frame>}
            if msg_topic == topic {
                if let Some(frame) = payload.get("payload") {
                    let mut line = serde_json::to_string(frame).unwrap_or_default();
                    line.push('\n');
                    to_daemon_w
                        .write_all(line.as_bytes())
                        .map_err(|e| format!("pipe write: {e}"))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn ws_send(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    text: &str,
) -> Result<(), String> {
    use tungstenite::Message;
    ws.write(Message::Text(text.into()))
        .map_err(|e| format!("ws send: {e}"))?;
    ws.flush().map_err(|e| format!("ws flush: {e}"))?;
    Ok(())
}

/// Put the socket fd into non-blocking mode so ws.read() surfaces
/// WouldBlock instead of stalling the poll loop.
fn set_nonblocking(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(format!("fcntl: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!("fcntl: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn next_ref() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn instant_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
