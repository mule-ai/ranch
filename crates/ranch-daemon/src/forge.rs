//! Forge (../forge) integration: chat panes bound to forge sessions.
//!
//! A forge-chat pane is NOT a PTY — the conversation lives in forge's
//! `messages` table (the source of truth). The daemon polls
//! `GET /messages` on a worker thread (blocking HTTP must never stall
//! the single-threaded poll loop) and writes `Frame::Chat` broadcasts
//! into a pipe, exactly like the relay thread does. Client sends go
//! out as `Frame::ChatSend`, which this module POSTs to
//! `POST /messages` (202 ACCEPTED; the rows come back through the
//! poll).
//!
//! Auth: `forge_api_key` in ~/.config/ranch/daemon.toml (forge login
//! returns it; X-API-Key header). `forge_profile_id` is optional —
//! the first profile is used when absent.

use ranch_protocol::{encode_frame, ChatMsg, Frame};
use crate::relay::load_config;
use std::io::{Read as _, Write as _};
use std::sync::mpsc;
use std::time::Duration;

pub struct ForgeConfig {
    pub base: String,
    pub key: String,
    pub profile: Option<String>,
}

/// Extract the forge section from daemon.toml (optional keys).
pub fn load_forge_config() -> Option<ForgeConfig> {
    // load_config returns None when daemon.toml is missing entirely;
    // forge config rides in the same file.
    load_config()?;
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
    /// Start polling a chat pane's forge session.
    Watch { pane: Uuid, forge_sid: Uuid },
    /// Stop polling (pane killed).
    Unwatch { pane: Uuid },
    /// POST a user message (spawns/wakes pi inside forge).
    Send { pane: Uuid, forge_sid: Uuid, text: String },
}

use uuid::Uuid;

struct Watch {
    pane: Uuid,
    forge_sid: Uuid,
    /// highest message `sequence` seen
    last_seq: i64,
}

/// One-shot JSON request helper (blocking; worker thread only).
fn http_json(
    cfg: &ForgeConfig,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", cfg.base, path);
    let method = method.to_ascii_uppercase();
    // ureq 3 types differ per body-ness — dispatch per method
    let sent = match (method.as_str(), body) {
        ("GET", _) => ureq::get(&url).header("X-API-Key", &cfg.key).call(),
        ("DELETE", _) => ureq::delete(&url).header("X-API-Key", &cfg.key).call(),
        ("POST", b) => ureq::post(&url)
            .header("X-API-Key", &cfg.key)
            .send_json(b.cloned().unwrap_or(serde_json::Value::Null))
            .map(|r| r),
        _ => return Err(format!("forge: unsupported method {method}")),
    };
    let mut res = sent.map_err(|e| format!("forge {method} {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("forge read {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("forge decode {path}: {e}"))
}

/// Extract message rows from `GET /messages?session_id=` responses.
/// The response shape has evolved; accept either a bare array or
/// `{messages: [...]}`.
fn rows_of(v: &serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(a) => a.clone(),
        other => other
            .get("messages")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default(),
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

fn write_frame(w: &mut std::fs::File, frame: &Frame) {
    let cid = uuid::Uuid::new_v4().to_string();
    for line in encode_frame(frame, &cid) {
        if w.write_all(line.as_bytes()).is_err() || w.write_all(b"\n").is_err() {
            return;
        }
    }
    let _ = w.flush();
}

/// Poll all watches; append-only diffs go out as Chat frames.
fn poll_all(cfg: &ForgeConfig, watches: &mut Vec<Watch>, w: &mut std::fs::File) {
    for watch in watches.iter_mut() {
        let path = format!("/messages?session_id={}", watch.forge_sid);
        let Ok(v) = http_json(cfg, "GET", &path, None) else {
            continue; // forge down or pane gone — retry next tick
        };
        let rows = rows_of(&v);
        let fresh: Vec<ChatMsg> = rows
            .iter()
            .filter_map(to_chat_msg)
            .filter(|m| m.seq > watch.last_seq)
            .collect();
        if fresh.is_empty() {
            continue;
        }
        if let Some(mx) = fresh.iter().map(|m| m.seq).max() {
            watch.last_seq = mx;
        }
        write_frame(
            w,
            &Frame::Chat {
                id: String::new(),
                session: String::new(), // filled by the main loop
                pane: watch.pane.to_string(),
                msgs: fresh,
                reset: false,
            },
        );
    }
}

/// Worker thread: owns all blocking forge HTTP. Job channel in,
/// `Frame::Chat` lines out through the pipe.
pub fn spawn_worker(
    cfg: ForgeConfig,
    mut pipe_w: std::fs::File,
    rx: mpsc::Receiver<ForgeJob>,
) {
    std::thread::spawn(move || {
        let mut watches: Vec<Watch> = Vec::new();
        loop {
            // drain pending jobs; timeout makes the poll tick
            let mut did_work = false;
            loop {
                match rx.recv_timeout(Duration::from_millis(if did_work { 50 } else { 1100 })) {
                    Ok(job) => {
                        did_work = true;
                        match job {
                            ForgeJob::Watch { pane, forge_sid } => {
                                // re-watch replaces (resets seq so the
                                // next poll sends the full history)
                                watches.retain(|w| w.pane != pane);
                                watches.push(Watch { pane, forge_sid, last_seq: 0 });
                            }
                            ForgeJob::Unwatch { pane } => {
                                watches.retain(|w| w.pane != pane);
                            }
                            ForgeJob::Send { pane, forge_sid, text } => {
                                let _ = http_json(
                                    &cfg,
                                    "POST",
                                    "/messages",
                                    Some(&serde_json::json!({
                                        "session_id": forge_sid.to_string(),
                                        "content": text,
                                    })),
                                );
                                // rows land via the poll; nudge it soon
                                // by shortening the next recv timeout
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            if !watches.is_empty() {
                poll_all(&cfg, &mut watches, &mut pipe_w);
            }
        }
    });
}

/// Create a forge session (sync, localhost — called from the main loop).
/// Returns the forge session uuid.
pub fn create_forge_session(cfg: &ForgeConfig, title: &str) -> Result<Uuid, String> {
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
    let v = http_json(
        cfg,
        "POST",
        "/sessions",
        Some(&serde_json::json!({ "profile_id": profile_id, "title": title })),
    )?;
    // response wraps the session: {session: {...}, working_dir: "..."}
    let sess = v.get("session").unwrap_or(&v);
    let id = sess
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or("POST /sessions: no id")?;
    Uuid::parse_str(id).map_err(|e| format!("forge session id: {e}"))
}
