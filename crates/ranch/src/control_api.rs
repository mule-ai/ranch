//! Loopback control API (Phase A): the HTTP surface local-pi agents
//! use to reach the daemon's agent tools.
//!
//! Why HTTP: pi extensions are JS (no unix sockets without native
//! deps), and forge's own `forge-tools` extension already proved the
//! pattern (pi extension + `pi.registerToolProvider` + HTTP).
//!
//! Security: the listener binds `127.0.0.1` only; every request must
//! carry the per-daemon bearer token (`ranchctl-<hex>`, handed to the
//! agent child via `RANCH_CONTROL_TOKEN`). The calling agent identifies
//! its pane via the `X-Ranch-Pane` header; missing/invalid = nil =
//! human = unrestricted (safe default on a loopback-only listener).
//!
//! Flow: the accept thread parses HTTP, builds a `ControlRequest`
//! (Frame + caller pane + reply channel) and sends it into the
//! returned channel. The daemon's single-threaded poll loop drains it
//! — handling it as a transient pseudo-client whose replies are
//! collected in a sink — and sends the reply frames back through the
//! reply channel; the accept thread serializes them into the HTTP
//! response. The agent's tool call blocks until the daemon replies
//! (correct: that's what a synchronous tool call is).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, Sender};

/// A control request parsed off the wire.
pub struct ControlRequest {
    /// pane the calling agent runs in (ownership anchor; nil = human)
    pub caller_pane: uuid::Uuid,
    pub frame: ranch_protocol::Frame,
    /// the daemon loop sends the result frames here
    pub reply: Sender<Vec<ranch_protocol::Frame>>,
}

/// Bind 127.0.0.1:0, spawn the accept thread, return (port, rx).
pub fn spawn() -> Result<(u16, Receiver<ControlRequest>), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("control bind: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = match std::env::var("RANCH_CONTROL_TOKEN") {
        // reuse a pre-set token when present (tests, operators);
        // otherwise mint one per daemon
        Ok(t) if !t.is_empty() => t,
        _ => format!("ranchctl-{}", uuid::Uuid::new_v4().simple()),
    };
    // agent children read these (pilocal exports them into the child env)
    // env is single-threaded at this point (daemon startup); the agent
    // children inherit these at spawn time
    unsafe {
        std::env::set_var("RANCH_CONTROL_TOKEN", &token);
        std::env::set_var("RANCH_CONTROL_PORT", port.to_string());
    }
    let (tx, rx) = std::sync::mpsc::channel::<ControlRequest>();
    std::thread::Builder::new()
        .name("ranchctl".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                handle_conn(&mut s, &token, &tx);
            }
        })
        .map_err(|e| format!("control thread: {e}"))?;
    Ok((port, rx))
}

/// Handle one connection: read one HTTP request, auth, parse, forward
/// to the daemon loop, serialize the reply. One request per connection
/// (agents use keep-it-simple clients; curl/pi-fetch reconnect fine).
fn handle_conn(
    s: &mut std::net::TcpStream,
    token: &str,
    tx: &Sender<ControlRequest>,
) {
    let Some((caller, frame, body_buf_len)) = read_and_parse(s, token) else {
        return; // response already written on error paths
    };
    let _ = body_buf_len;
    let (rtx, rrx) = std::sync::mpsc::channel();
    let req = ControlRequest {
        caller_pane: caller,
        frame,
        reply: rtx,
    };
    if tx.send(req).is_err() {
        let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
        return;
    }
    let frames = rrx
        .recv_timeout(std::time::Duration::from_secs(300))
        .unwrap_or_default();
    let body = serde_json::to_string(&frames).unwrap_or_else(|_| "[]".into());
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = s.write_all(resp.as_bytes());
}

/// Read + parse one HTTP request. Returns (caller_pane, frame) on
/// success; writes the error response and returns None otherwise.
fn read_and_parse(
    s: &mut std::net::TcpStream,
    token: &str,
) -> Option<(uuid::Uuid, ranch_protocol::Frame, usize)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return None,
        }
        if buf.len() > 1 << 20 {
            let _ = s.write_all(b"HTTP/1.1 413 Payload Too Large\r\ncontent-length: 0\r\n\r\n");
            return None; // 1 MB cap
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let req_line = lines.next()?; // "POST /agent/<tool> HTTP/1.1"
    let mut auth_ok = false;
    let mut content_length = 0usize;
    let mut caller = uuid::Uuid::nil();
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("authorization: bearer ") {
            auth_ok = v.trim() == token;
        }
        if let Some(v) = lower.strip_prefix("x-ranch-pane:") {
            caller = uuid::Uuid::parse_str(v.trim()).unwrap_or(uuid::Uuid::nil());
        }
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if !auth_ok {
        let _ = s.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
        return None;
    }
    let header_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);

    // route: POST /agent/<tool>
    let path = req_line.split_whitespace().nth(1).unwrap_or("");
    let tool = path.trim_start_matches("/agent/");
    let frame = build_frame(tool, &body, caller)?;
    Some((caller, frame, body.len()))
}

/// Build the Frame for one control route.
fn build_frame(tool: &str, body: &[u8], caller: uuid::Uuid) -> Option<ranch_protocol::Frame> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let get_opt = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    match tool {
        "spawn" => Some(ranch_protocol::Frame::AgentSpawn {
            req_id: uuid::Uuid::new_v4().to_string(),
            caller_pane: caller.to_string(),
            caller_session: get("session"),
            kind: get("kind"),
            profile_id: get_opt("profile_id"),
            name: get_opt("name"),
            cwd: get_opt("cwd"),
            prompt: get("prompt"),
            mode: if get("mode") == "session" {
                "session".into()
            } else {
                "split".into()
            },
            callback: v.get("callback").and_then(|x| x.as_bool()).unwrap_or(true),
        }),
        "send" => Some(ranch_protocol::Frame::AgentSend {
            req_id: uuid::Uuid::new_v4().to_string(),
            caller_pane: caller.to_string(),
            session: String::new(),
            pane: get("pane"),
            text: get("text"),
            delivery: if get("delivery") == "queue" {
                "queue".into()
            } else {
                "steer".into()
            },
        }),
        "status" => Some(ranch_protocol::Frame::AgentStatus {
            req_id: uuid::Uuid::new_v4().to_string(),
            caller_pane: caller.to_string(),
            pane: get("pane"),
        }),
        "read" => Some(ranch_protocol::Frame::AgentRead {
            req_id: uuid::Uuid::new_v4().to_string(),
            caller_pane: caller.to_string(),
            pane: get("pane"),
            since_seq: v.get("since_seq").and_then(|x| x.as_i64()).unwrap_or(0),
            limit: v.get("limit").and_then(|x| x.as_u64()).unwrap_or(50) as u32,
        }),
        "close" => Some(ranch_protocol::Frame::AgentClose {
            req_id: uuid::Uuid::new_v4().to_string(),
            caller_pane: caller.to_string(),
            session: String::new(),
            pane: get("pane"),
        }),
        _ => None,
    }
}
