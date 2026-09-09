//! Forge (../forge) integration: chat panes bound to forge sessions.
//!
//! A forge-chat pane is NOT a PTY — the conversation lives in forge's
//! `messages` table (the source of truth). One worker thread per
//! watched forge session streams `GET /sessions/{id}/events?since=`
//! (SSE: `message` rows, catch-up on connect/reconnect, heartbeats)
//! and writes `Frame::Chat` broadcasts into a pipe read by the main
//! loop like any client (the relay-pipe pattern). Client sends go out
//! as `Frame::ChatSend`, POSTed to `POST /messages` on the job thread
//! (blocking HTTP must never stall the single-threaded poll loop).
//!
//! Auth: `forge_api_key` in ~/.config/ranch/daemon.toml (forge login
//! returns it; X-API-Key header). `forge_profile_id` is optional —
//! the first profile is used when absent.

use ranch_protocol::{encode_frame, ChatMsg, Frame};
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::RecvError;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct ForgeConfig {
    pub base: String,
    pub key: String,
    pub profile: Option<String>,
}

/// Extract the forge section from daemon.toml (optional keys).
pub fn load_forge_config() -> Option<ForgeConfig> {
    // forge config rides in daemon.toml (same file as the relay);
    // forge keys are optional — integration is off when absent
    let home = std::env::var("HOME").ok()?;
    let text = std::fs::read_to_string(
        std::path::PathBuf::from(home).join(".config/ranch/daemon.toml"),
    )
    .ok()?;
    let get = |key: &str| -> String {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(key) {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    return rest.trim().trim_matches('"').to_string();
                }
            }
        }
        String::new()
    };
    let key = get("forge_api_key");
    if key.is_empty() {
        return None; // forge integration disabled
    }
    let base = {
        let b = get("forge_url");
        if b.is_empty() { "http://127.0.0.1:8080".to_string() } else { b }
    };
    let profile = get("forge_profile_id");
    Some(ForgeConfig {
        base,
        key,
        profile: if profile.is_empty() { None } else { Some(profile) },
    })
}

/// Jobs the main loop hands to the worker.
pub enum ForgeJob {
    /// Start streaming a chat pane's forge session.
    Watch { pane: Uuid, forge_sid: Uuid },
    /// Stop streaming (pane killed).
    Unwatch { pane: Uuid },
    /// POST a user message (spawns/wakes pi inside forge).
    Send { pane: Uuid, forge_sid: Uuid, text: String },
}

/// Shared per-watch state: the SSE thread owns one; the job thread
/// flips `stop` on Unwatch.
struct WatchState {
    pane: Uuid,
    forge_sid: Uuid,
    /// highest message `sequence` delivered (dedup across reconnects —
    /// forge re-sends catch-up rows after every reconnect and
    /// dedupes by sequence server-side too)
    last_seq: AtomicI64,
    stop: AtomicBool,
}

/// Shared pipe writer (SSE threads + job thread all emit frames).
type PipeWriter = Arc<Mutex<std::fs::File>>;

fn write_frame(w: &PipeWriter, frame: &Frame) {
    let cid = Uuid::new_v4().to_string();
    let mut lines = Vec::new();
    for line in encode_frame(frame, &cid) {
        lines.push(line);
        lines.push("\n".into());
    }
    if let Ok(mut f) = w.lock() {
        let _ = f.write_all(lines.join("").as_bytes());
        let _ = f.flush();
    }
}

fn to_chat_msg(r: &serde_json::Value) -> Option<ChatMsg> {
    let seq = r.get("sequence")?.as_i64()?;
    let role = r.get("role")?.as_str()?.to_string();
    let text = r
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let tool_name = r.get("tool_name").and_then(|t| t.as_str()).map(String::from);
    let tool_call_id = r.get("tool_call_id").and_then(|t| t.as_str()).map(String::from);
    let tool_output = r.get("tool_output").and_then(|t| {
        if t.is_null() {
            None
        } else {
            Some(match t.as_str() {
                Some(s) => s.to_string(),
                None => t.to_string(),
            })
        }
    });
    let duration_ms = r.get("duration_ms").and_then(|d| d.as_i64());
    let created_at = r.get("created_at").and_then(|c| c.as_str()).map(String::from);
    // skip empty rows (e.g. assistant rows that only carried tool_input)
    if text.is_empty() && tool_name.is_none() && tool_call_id.is_none() {
        return None;
    }
    Some(ChatMsg {
        seq,
        role,
        text,
        tool_name,
        tool_call_id,
        tool_output,
        duration_ms,
        created_at,
    })
}

/// One SSE connection per watched forge session. Streams `message`
/// events; each triggers a Chat frame with the (deduped) new rows.
/// Reconnects with backoff — `since=` on reconnect gives server-side
/// catch-up, and the last_seq high-water mark dedupes locally.
fn run_sse(state: Arc<WatchState>, cfg: ForgeConfig, w: PipeWriter) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        if state.stop.load(Ordering::Relaxed) {
            return;
        }
        let url = format!(
            "{}/sessions/{}/events?since={}",
            cfg.base,
            state.forge_sid,
            state.last_seq.load(Ordering::Relaxed)
        );
        let resp = ureq::get(&url)
            .header("X-API-Key", &cfg.key)
            .header("Accept", "text/event-stream")
            .call();
        match resp {
            Ok(res) => {
                eprintln!("forge-sse: connected to {}", state.forge_sid);
                backoff = std::time::Duration::from_secs(1);
                // stream lines: SSE frames are `event: <name>` +
                // `data: <json>` + blank line. buf borrows res and
                // drops before it (reverse declaration order).
                let mut res = res;
                let mut buf = std::io::BufReader::new(res.body_mut().as_reader());
                let mut event_name = String::new();
                let mut data = String::new();
                loop {
                    if state.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut line = String::new();
                    match buf.read_line(&mut line) {
                        Ok(0) => break, // stream closed
                        Ok(_) => {
                            let line = line.trim_end().to_string();
                            if let Some(name) = line.strip_prefix("event:") {
                                event_name = name.trim().to_string();
                            } else if let Some(d) = line.strip_prefix("data:") {
                                data.push_str(d.trim());
                            } else if line.is_empty() {
                                // dispatch
                                handle_event(&state, &cfg, &w, &event_name, &data);
                                event_name.clear();
                                data.clear();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(e) => {
                eprintln!("forge-sse: connect failed: {e}");
            }
        }
        if state.stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
    }
}

fn handle_event(state: &Arc<WatchState>, cfg: &ForgeConfig, w: &PipeWriter, name: &str, data: &str) {
    match name {
        "message" => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return };
            let Some(msg) = to_chat_msg(&v) else { return };
            let last = state.last_seq.load(Ordering::Relaxed);
            if msg.seq <= last {
                return; // reconnect catch-up duplicate
            }
            state.last_seq.store(msg.seq, Ordering::Relaxed);
            write_frame(
                w,
                &Frame::Chat {
                    id: String::new(),
                    session: String::new(), // filled by the main loop
                    pane: state.pane.to_string(),
                    msgs: vec![msg],
                    reset: false,
                },
            );
        }
        "turn_ended" | "heartbeat" | "lagged" => {
            // lagged: forge already backfilled the missed rows as
            // `message` events before this one — nothing to do
        }
        _ => {}
    }
    let _ = cfg;
}

/// Job thread: watches registry + blocking POSTs for sends.
pub fn spawn_worker(
    cfg: ForgeConfig,
    pipe_w: std::fs::File,
    rx: mpsc::Receiver<ForgeJob>,
) {
    let pipe: PipeWriter = Arc::new(Mutex::new(pipe_w));
    std::thread::spawn(move || {
        let mut threads: Vec<(Uuid, Arc<WatchState>)> = Vec::new();
        loop {
            match rx.recv() {
                Ok(job) => match job {
                    ForgeJob::Watch { pane, forge_sid } => {
                        // re-watch replaces (fresh high-water mark)
                        if let Some((_, old)) = threads
                            .iter()
                            .position(|(_, st)| st.pane == pane)
                            .map(|i| threads.remove(i))
                        {
                            old.stop.store(true, Ordering::Relaxed);
                        }
                        let st = Arc::new(WatchState {
                            pane,
                            forge_sid,
                            last_seq: AtomicI64::new(0),
                            stop: AtomicBool::new(false),
                        });
                        let t_cfg = ForgeConfig {
                            base: cfg.base.clone(),
                            key: cfg.key.clone(),
                            profile: cfg.profile.clone(),
                        };
                        let t_pipe = pipe.clone();
                        let t_state = st.clone();
                        std::thread::spawn(move || {
                            run_sse(t_state, t_cfg, t_pipe);
                        });
                        threads.push((pane, st));
                    }
                    ForgeJob::Unwatch { pane } => {
                        if let Some(i) = threads.iter().position(|(_, st)| st.pane == pane) {
                            let (_, st) = threads.remove(i);
                            st.stop.store(true, Ordering::Relaxed);
                        }
                    }
                    ForgeJob::Send { pane, forge_sid, text } => {
                        let _ = http_post_message(&cfg, forge_sid, &text);
                        // rows land via the SSE stream
                    }
                },
                Err(mpsc::RecvError) => return,
            }
        }
    });
}

/// Send a user message (job thread only — blocking).
fn http_post_message(cfg: &ForgeConfig, forge_sid: Uuid, text: &str) -> Result<(), String> {
    let url = format!("{}/messages", cfg.base);
    let _ = ureq::post(&url)
        .header("X-API-Key", &cfg.key)
        .send_json(serde_json::json!({
            "session_id": forge_sid.to_string(),
            "content": text,
        }))
        .map_err(|e| format!("forge POST /messages: {e}"))?;
    Ok(())
}

/// One-shot JSON request helper (sync; main loop only — localhost).
fn http_json(
    cfg: &ForgeConfig,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", cfg.base, path);
    let m = method.to_ascii_uppercase();
    let sent = match (m.as_str(), body) {
        ("GET", _) => ureq::get(&url).header("X-API-Key", &cfg.key).call(),
        ("POST", b) => ureq::post(&url)
            .header("X-API-Key", &cfg.key)
            .send_json(b.cloned().unwrap_or(serde_json::Value::Null)),
        _ => return Err(format!("forge: unsupported method {method}")),
    };
    let mut res = sent.map_err(|e| format!("forge {method} {path}: {e}"))?;
    let mut text = String::new();
    use std::io::Read as _;
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("forge read {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("forge decode {path}: {e}"))
}

/// Create a forge session (sync, localhost — called from the main loop).
/// `working_dir` anchors the agent to an existing directory (forge
/// migration 014) — e.g. the terminal pane's cwd for agent splits.
/// Returns the forge session uuid.
pub fn create_forge_session(cfg: &ForgeConfig, title: &str, working_dir: Option<&str>) -> Result<Uuid, String> {
    // profile: configured or the first one
    let profile_id = match &cfg.profile {
        Some(p) => p.clone(),
        None => {
            let v = http_json(cfg, "GET", "/profiles", None)?;
            // response is {profiles: [...]} (or a bare array)
            let arr = v
                .get("profiles")
                .and_then(|p| p.as_array())
                .cloned()
                .or_else(|| v.as_array().cloned())
                .ok_or("GET /profiles: unexpected shape")?;
            arr.first()
                .and_then(|p| p.get("id"))
                .and_then(|i| i.as_str())
                .ok_or("no forge profiles")?
                .to_string()
        }
    };
    let mut body = serde_json::json!({ "profile_id": profile_id, "title": title });
    if let Some(dir) = working_dir {
        body["working_dir"] = serde_json::Value::String(dir.to_string());
    }
    let v = http_json(cfg, "POST", "/sessions", Some(&body))?;
    // response wraps the session: {session: {...}, working_dir: "..."}
    let sess = v.get("session").unwrap_or(&v);
    let id = sess
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or("POST /sessions: no id")?;
    Uuid::parse_str(id).map_err(|e| format!("forge session id: {e}"))
}
