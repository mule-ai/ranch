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

use ranch_protocol::{ChatMsg, Frame, ModelChoice, encode_frame};
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::RecvError;
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
    let text =
        std::fs::read_to_string(std::path::PathBuf::from(home).join(".config/ranch/daemon.toml"))
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
        if b.is_empty() {
            "http://127.0.0.1:8080".to_string()
        } else {
            b
        }
    };
    let profile = get("forge_profile_id");
    Some(ForgeConfig {
        base,
        key,
        profile: if profile.is_empty() {
            None
        } else {
            Some(profile)
        },
    })
}

/// Jobs the main loop hands to the worker.
pub enum ForgeJob {
    /// Start streaming a chat pane's forge session.
    Watch { pane: Uuid, forge_sid: Uuid },
    /// Stop streaming (pane killed).
    Unwatch { pane: Uuid },
    /// POST a user message (spawns/wakes pi inside forge).
    Send {
        pane: Uuid,
        forge_sid: Uuid,
        text: String,
    },
    /// List resumable forge sessions (GET /sessions).
    List { req_id: String },
    /// Model catalog + effective model for a chat pane's forge session.
    ModelList { pane: Uuid, forge_sid: Uuid, req_id: String },
    /// Switch the session's model (PATCH /sessions/:id model switcher).
    ModelSet {
        pane: Uuid,
        forge_sid: Uuid,
        req_id: String,
        provider: String,
        model: String,
    },
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
    /// bumped on every "working" signal; delayed idle timers no-op if
    /// the generation moved on (a new turn started)
    turn_gen: AtomicU64,
}

/// Shared pipe writer (SSE threads + job thread all emit frames).
pub type PipeWriter = Arc<Mutex<std::fs::File>>;

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
        .trim()
        .to_string();
    let tool_name = r
        .get("tool_name")
        .and_then(|t| t.as_str())
        .map(String::from);
    let tool_call_id = r
        .get("tool_call_id")
        .and_then(|t| t.as_str())
        .map(String::from);
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
    let created_at = r
        .get("created_at")
        .and_then(|c| c.as_str())
        .map(String::from);
    // skip empty rows (e.g. assistant rows that only carried tool_input)
    if text.is_empty() && tool_name.is_none() {
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

fn handle_event(
    state: &Arc<WatchState>,
    cfg: &ForgeConfig,
    w: &PipeWriter,
    name: &str,
    data: &str,
) {
    match name {
        "message" => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                return;
            };
            let Some(msg) = to_chat_msg(&v) else { return };
            let last = state.last_seq.load(Ordering::Relaxed);
            if msg.seq <= last {
                return; // reconnect catch-up duplicate
            }
            state.last_seq.store(msg.seq, Ordering::Relaxed);
            // a user row means the agent took the message; it works
            // until turn_ended. Clients show the working indicator.
            if msg.role == "user" {
                state.turn_gen.fetch_add(1, Ordering::Relaxed);
                write_agent_status(w, state.pane, "working");
            }
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
        "turn_ended" => {
            // agent finished the turn — but the assistant's message rows
            // typically land a beat AFTER turn_ended. Clear the working
            // indicator on a short delay so clients show the reply with
            // the indicator still up. A new turn bumps `turn_gen`, which
            // cancels a stale timer.
            let state = state.clone();
            let w = w.clone();
            let pane = state.pane;
            let before = state.turn_gen.load(Ordering::Relaxed);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                if state.stop.load(Ordering::Relaxed)
                    || state.turn_gen.load(Ordering::Relaxed) != before
                {
                    return; // cancelled: a new turn started
                }
                write_agent_status(&w, pane, "idle");
            });
        }
        "heartbeat" | "lagged" => {
            // lagged: forge already backfilled the missed rows as
            // `message` events before this one — nothing to do
        }
        _ => {}
    }
    let _ = cfg;
}

/// Agent busy/idle signal (client typing indicator).
fn write_agent_status(w: &PipeWriter, pane: Uuid, status: &str) {
    write_frame(
        w,
        &Frame::Meta {
            session: String::new(), // filled by the main loop
            pane: Some(pane.to_string()),
            kind: "agent".into(),
            status: Some(status.into()),
        },
    );
}

/// Job thread: watches registry + blocking POSTs for sends.
pub fn spawn_worker(cfg: ForgeConfig, pipe_w: std::fs::File, rx: mpsc::Receiver<ForgeJob>) {
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
                            turn_gen: AtomicU64::new(0),
                        });
                        let t_cfg = ForgeConfig {
                            base: cfg.base.clone(),
                            key: cfg.key.clone(),
                            profile: cfg.profile.clone(),
                        };
                        let t_pipe = pipe.clone();
                        let t_state = st.clone();
                        // current model (effective = override ?? profile) —
                        // report it so the chat-pane UI can show it
                        if let Some(cur) = effective_model(&t_cfg, forge_sid) {
                            write_frame(
                                &t_pipe,
                                &Frame::Meta {
                                    session: String::new(),
                                    pane: Some(pane.to_string()),
                                    kind: "model".into(),
                                    status: Some(cur.name),
                                },
                            );
                        }
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
                    ForgeJob::Send {
                        pane,
                        forge_sid,
                        text,
                    } => {
                        let _ = http_post_message(&cfg, forge_sid, &text);
                        // the working indicator starts as soon as the
                        // POST is accepted; rows land via the SSE stream
                        write_agent_status(&pipe, pane, "working");
                    }
                    ForgeJob::List { req_id } => match http_json(&cfg, "GET", "/sessions", None) {
                        Ok(v) => {
                            // response is {sessions: [...]} (or a bare array)
                            let arr = v
                                .get("sessions")
                                .and_then(|x| x.as_array())
                                .cloned()
                                .or_else(|| v.as_array().cloned())
                                .unwrap_or_default();
                            let sessions: Vec<ranch_protocol::ForgeSessionInfo> = arr
                                .iter()
                                .filter_map(|s| {
                                    let id = s.get("id")?.as_str()?.to_string();
                                    let title = s
                                        .get("title")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let updated = s
                                        .get("last_active")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let ended = s
                                        .get("ended_at")
                                        .and_then(|t| t.as_str())
                                        .map(|t| t.to_string());
                                    Some(ranch_protocol::ForgeSessionInfo {
                                        id,
                                        title,
                                        updated,
                                        ended,
                                    })
                                })
                                .collect();
                            let cid = uuid::Uuid::new_v4().to_string();
                            if let Ok(mut f) = pipe.lock() {
                                use std::io::Write as _;
                                for line in ranch_protocol::encode_frame(
                                    &ranch_protocol::Frame::ForgeListOk {
                                        id: String::new(),
                                        req_id: req_id.clone(),
                                        sessions,
                                    },
                                    &cid,
                                ) {
                                    let _ = f.write_all(line.as_bytes());
                                    let _ = f.write_all(b"\n");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("ranchd: forge list failed: {e}");
                            let cid = uuid::Uuid::new_v4().to_string();
                            if let Ok(mut f) = pipe.lock() {
                                use std::io::Write as _;
                                for line in ranch_protocol::encode_frame(
                                    &ranch_protocol::Frame::ForgeListOk {
                                        id: String::new(),
                                        req_id: req_id.clone(),
                                        sessions: Vec::new(),
                                    },
                                    &cid,
                                ) {
                                    let _ = f.write_all(line.as_bytes());
                                    let _ = f.write_all(b"\n");
                                }
                            }
                        }
                    },
                    ForgeJob::ModelList {
                        pane,
                        forge_sid,
                        req_id,
                    } => {
                        let (models, names) = fetch_catalog(&cfg);
                        let current = effective_model_from(&cfg, forge_sid, &names);
                        write_frame(
                            &pipe,
                            &Frame::ModelListOk {
                                id: String::new(),
                                req_id,
                                pane: pane.to_string(),
                                current,
                                models,
                            },
                        );
                    }
                    ForgeJob::ModelSet {
                        pane,
                        forge_sid,
                        req_id,
                        provider,
                        model,
                    } => {
                        let display = catalog_name(&cfg, &provider, &model);
                        match http_json(
                            &cfg,
                            "PATCH",
                            &format!("/sessions/{forge_sid}"),
                            Some(&serde_json::json!({
                                "provider": provider,
                                "model": model,
                            })),
                        ) {
                            Ok(_) => write_frame(
                                &pipe,
                                &Frame::Meta {
                                    session: String::new(),
                                    pane: Some(pane.to_string()),
                                    kind: "model".into(),
                                    status: Some(display),
                                },
                            ),
                            Err(e) => {
                                eprintln!("ranchd: forge model switch failed: {e}");
                                write_frame(
                                    &pipe,
                                    &Frame::Error {
                                        req_id: Some(req_id),
                                        message: format!("model switch failed: {e}"),
                                    },
                                );
                            }
                        }
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
        ("PATCH", b) => ureq::patch(&url)
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

/// Fetch pi's `models.json` catalog via forge's
/// `GET /v1/models/catalog` (secrets already stripped). Returns the
/// flattened model list plus a (provider, id) -> name lookup map.
fn fetch_catalog(
    cfg: &ForgeConfig,
) -> (Vec<ModelChoice>, std::collections::HashMap<(String, String), String>) {
    let mut models = Vec::new();
    let mut names: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let v = match http_json(cfg, "GET", "/v1/models/catalog", None) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ranchd: forge catalog fetch failed: {e}");
            return (models, names);
        }
    };
    if let Some(providers) = v.get("providers").and_then(|p| p.as_object()) {
        for (prov, pc) in providers {
            if let Some(arr) = pc.get("models").and_then(|m| m.as_array()) {
                for m in arr {
                    let Some(id) = m.get("id").and_then(|x| x.as_str()) else {
                        continue;
                    };
                    let name = m
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or(id)
                        .to_string();
                    let (prov_s, id_s) = (prov.clone(), id.to_string());
                    names.insert((prov_s.clone(), id_s.clone()), name.clone());
                    models.push(ModelChoice {
                        provider: prov_s,
                        id: id_s,
                        name,
                    });
                }
            }
        }
    }
    (models, names)
}

/// Friendly display name for (provider, model id) from the catalog,
/// falling back to the raw id.
fn catalog_name(cfg: &ForgeConfig, provider: &str, id: &str) -> String {
    let (_, names) = fetch_catalog(cfg);
    names
        .get(&(provider.to_string(), id.to_string()))
        .cloned()
        .unwrap_or_else(|| id.to_string())
}

/// Effective model of a forge session: `override_model ?? profile.model`
/// (forge's model-switcher semantics). None when the session/profile
/// can't be read (e.g. forge not up).
fn effective_model(cfg: &ForgeConfig, sid: Uuid) -> Option<ModelChoice> {
    let (_models, names) = fetch_catalog(cfg);
    effective_model_from(cfg, sid, &names)
}

fn effective_model_from(
    cfg: &ForgeConfig,
    sid: Uuid,
    names: &std::collections::HashMap<(String, String), String>,
) -> Option<ModelChoice> {
    let s = http_json(cfg, "GET", &format!("/sessions/{sid}"), None)
        .ok()
        .and_then(|v| v.get("session").cloned())?;
    let ov_prov = s.get("override_provider").and_then(|x| x.as_str());
    let ov_mod = s.get("override_model").and_then(|x| x.as_str());
    let (prov, id) = match (ov_prov, ov_mod) {
        (Some(p), Some(m)) => (p.to_string(), m.to_string()),
        _ => {
            let pid = s.get("profile_id")?.as_str()?;
            let prof = http_json(cfg, "GET", &format!("/profiles/{pid}"), None)
                .ok()
                .and_then(|v| v.get("profile").cloned())?;
            (
                prof.get("provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                prof.get("model")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        }
    };
    let name = names
        .get(&(prov.clone(), id.clone()))
        .cloned()
        .unwrap_or_else(|| id.clone());
    Some(ModelChoice {
        provider: prov,
        id,
        name,
    })
}

/// Create a forge session (sync, localhost — called from the main loop).
/// `working_dir` anchors the agent to an existing directory (forge
/// migration 014) — e.g. the terminal pane's cwd for agent splits.
/// Returns the forge session uuid.
pub fn create_forge_session(
    cfg: &ForgeConfig,
    title: &str,
    working_dir: Option<&str>,
) -> Result<Uuid, String> {
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
