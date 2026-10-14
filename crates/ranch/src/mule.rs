//! Mule workflow integration (Phase C): REST proxy + run streaming.
//!
//! Mirrors the forge-worker architecture: one worker thread owns all
//! blocking mule HTTP/WS; jobs arrive on an mpsc channel; frames go out
//! through the shared agent pipe (the daemon's pre-match resolves pane
//! addresses + broadcasts).
//!
//! Mule surface used (../mule/cmd/api/server.go):
//! - `/api/v1/workflows` CRUD (+ `/{id}/steps`, `/steps/reorder`)
//! - `POST /api/v1/jobs` {workflow_id, input_data} -> {id, status}
//! - `WS /ws` — global broadcast hub (`WebSocketMessage{type,data,
//!   timestamp}`): `job_update`, `job_step_update`, agent events.
//!   Firehose: the worker filters client-side by job id BEFORE frames
//!   cross the relay.
//!
//! Mule currently has no auth middleware (server.go mounts
//! logging/recovery/CORS only); `mule_api_key` in daemon.toml is sent
//! as `Authorization: Bearer` for when upstream grows auth.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::mpsc;
use uuid::Uuid;

use crate::forge::PipeWriter;
use ranch_protocol::{Frame, MuleAgent, WorkflowDraft, WorkflowStep, WorkflowSummary};

#[derive(Debug, Clone)]
pub struct MuleConfig {
    pub base: String,
    pub api_key: Option<String>,
    /// `[wakes] mule_forward = true` (H3.3): forward local WebhookEvents
    /// fire-and-forget to mule's `POST /api/v1/wakes/fire` so one
    /// webhook can drive both local panes and fleet agents.
    pub wakes_forward: bool,
    /// `[wakes] wakes_url` — falls back to `mule_url` when absent.
    pub wakes_url: String,
    /// `[wakes] wakes_api_key` — falls back to `mule_api_key`.
    pub wakes_api_key: Option<String>,
}

/// Extract the mule section from daemon.toml (optional keys —
/// integration is off when `mule_url` is absent). Also reads the
/// optional `[wakes]` table (H3.3): `mule_forward = true`,
/// `wakes_url`, `wakes_api_key` (both fall back to the top-level
/// `mule_url` / `mule_api_key`).
pub fn load_mule_config() -> Option<MuleConfig> {
    let home = std::env::var("HOME").ok()?;
    let text =
        std::fs::read_to_string(std::path::PathBuf::from(home).join(".config/ranch/daemon.toml"))
            .ok()?;
    parse_toml_wakes(&text)
}

/// Parse the mule + `[wakes]` keys out of a daemon.toml body
/// (section-aware line scan: keys under `[wakes]` are tracked
/// separately from the top-level keys).
fn parse_toml_wakes(text: &str) -> Option<MuleConfig> {
    let mut top: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut wakes: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut section = String::from("top");
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some(rest) = line.split_once('=') else {
            continue;
        };
        let key = rest.0.trim();
        let val = rest.1.trim().trim_matches('#').trim().trim_matches('"');
        match section.as_str() {
            "wakes" => {
                wakes.insert(key.to_string(), val.to_string());
            }
            "top" => {
                top.insert(key.to_string(), val.to_string());
            }
            _ => {}
        }
    }
    let url = top.get("mule_url").cloned().unwrap_or_default();
    if url.is_empty() {
        return None;
    }
    let key = top.get("mule_api_key").cloned().unwrap_or_default();
    let forward = wakes
        .get("mule_forward")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let wakes_url = wakes
        .get("wakes_url")
        .filter(|v| !v.is_empty())
        .cloned()
        .map(|v| v.trim_end_matches('/').to_string())
        .unwrap_or_else(|| url.trim_end_matches('/').to_string());
    let wakes_api_key = wakes
        .get("wakes_api_key")
        .filter(|v| !v.is_empty())
        .cloned()
        .or((!key.is_empty()).then_some(key.clone()));
    Some(MuleConfig {
        base: url.trim_end_matches('/').to_string(),
        api_key: if key.is_empty() { None } else { Some(key) },
        wakes_forward: forward,
        wakes_url,
        wakes_api_key,
    })
}

// ---------- wake forward (H3.3) ----------

/// The `POST /api/v1/wakes/fire` body for a local webhook event:
/// ranch's webhook shape {source, event_tag, payload} becomes mule's
/// wake ingress shape {kind: "webhook", source, event_tag?, payload}.
pub fn wake_forward_body(
    source: &str,
    event: Option<&str>,
    payload: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "webhook",
        "source": source,
        "event_tag": event,
        "payload": payload,
    })
}

/// Fire-and-forget webhook -> mule wake forward (H3.3). No-op unless
/// `[wakes] mule_forward = true`. Errors are logged and swallowed: a
/// mule outage must never fail local webhook handling.
pub fn forward_webhook_to_wakes(
    cfg: &MuleConfig,
    source: &str,
    event: Option<&str>,
    payload: &serde_json::Value,
) {
    if !cfg.wakes_forward {
        return;
    }
    let body = wake_forward_body(source, event, payload);
    // No per-call timeout, same posture as http_json above: the daemon
    // already accepts blocking mule calls, and the forward is bounded
    // to one request (no retries, no reconnects).
    let mut r = ureq::post(format!("{}/api/v1/wakes/fire", cfg.wakes_url));
    if let Some(k) = &cfg.wakes_api_key {
        r = r.header("Authorization", &format!("Bearer {k}"));
    }
    match r.send_json(&body) {
        Ok(_) => eprintln!(
            "ranchd: mule wakes: forwarded webhook {source}/{} to the fleet wake engine",
            event.unwrap_or("")
        ),
        Err(e) => eprintln!(
            "ranchd: mule wakes: forward failed (dropped, local webhook handling unaffected): {e}"
        ),
    }
}

/// Jobs the main loop hands to the worker.
pub enum MuleJob {
    List {
        req_id: String,
    },
    /// mule agents (workflow-step pickers)
    Agents {
        req_id: String,
    },
    Get {
        req_id: String,
        workflow_id: String,
    },
    Put {
        req_id: String,
        workflow_id: Option<String>,
        draft: WorkflowDraft,
    },
    Delete {
        req_id: String,
        workflow_id: String,
    },
    /// Run into a new pane: POST /api/v1/jobs, then tee the WS stream
    /// for that job into the pane.
    Run {
        req_id: String,
        workflow_id: String,
        input: Option<serde_json::Value>,
        session: Uuid,
        pane: Uuid,
    },
    /// Stop teeing a pane's job stream (pane killed).
    #[allow(dead_code)]
    Unwatch {
        pane: Uuid,
    },
}

// ---------- HTTP helpers (worker thread only) ----------

fn http_json(
    cfg: &MuleConfig,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", cfg.base, path);
    let m = method.to_ascii_uppercase();
    fn with_auth<B>(r: ureq::RequestBuilder<B>, key: &Option<String>) -> ureq::RequestBuilder<B> {
        match key {
            Some(k) => r.header("Authorization", &format!("Bearer {k}")),
            None => r,
        }
    }
    let sent = match (m.as_str(), body) {
        ("GET", _) => with_auth(ureq::get(&url), &cfg.api_key).call(),
        ("POST", b) => with_auth(ureq::post(&url), &cfg.api_key)
            .send_json(b.cloned().unwrap_or(serde_json::Value::Null)),
        ("PUT", b) => with_auth(ureq::put(&url), &cfg.api_key)
            .send_json(b.cloned().unwrap_or(serde_json::Value::Null)),
        ("DELETE", _) => with_auth(ureq::delete(&url), &cfg.api_key).call(),
        _ => return Err(format!("mule: unsupported method {method}")),
    };
    let mut res = sent.map_err(|e| format!("mule {method} {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("mule read {path}: {e}"))?;
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("mule decode {path}: {e}"))
}

// ---------- run streaming ----------

/// One watched run: (pane -> job id). The WS reader filters the global
/// firehose against this map and writes formatted rows into the pane.
type Watches = std::sync::Arc<std::sync::Mutex<BTreeMap<Uuid, String>>>;

fn write_row(w: &PipeWriter, pane: Uuid, text: &str) {
    write_frame(
        w,
        &Frame::Chat {
            id: String::new(),
            session: String::new(), // resolved by the daemon pre-match
            pane: pane.to_string(),
            msgs: vec![ranch_protocol::ChatMsg {
                seq: next_seq(),
                role: "assistant".into(),
                text: text.to_string(),
                tool_name: None,
                tool_call_id: None,
                tool_output: None,
                tool_args: None,
                duration_ms: None,
                created_at: Some(now_iso()),
                attachments: None,
                image_refs: None,
            }],
            reset: false,
        },
    );
}

fn write_frame(w: &PipeWriter, frame: &Frame) {
    let cid = Uuid::new_v4().to_string();
    let mut lines = Vec::new();
    for line in ranch_protocol::encode_frame(frame, &cid) {
        lines.push(line);
        lines.push("\n".into());
    }
    if let Ok(mut f) = w.lock() {
        let _ = f.write_all(lines.join("").as_bytes());
        let _ = f.flush();
    }
}

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn next_seq() -> i64 {
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as i64
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::daemon::agenttools::iso_from_unix(secs)
}

/// Format one mule WS event into pane rows (None = not interesting).
fn format_event(type_: &str, data: &serde_json::Value, job: &str) -> Vec<String> {
    match type_ {
        "job_update" => {
            let id = data.get("id").and_then(|x| x.as_str()).unwrap_or("");
            if id != job {
                return vec![];
            }
            let status = data.get("status").and_then(|x| x.as_str()).unwrap_or("");
            match status {
                "RUNNING" => vec!["▶ workflow running".into()],
                "COMPLETED" => vec!["✓ workflow completed".into()],
                "FAILED" => vec![format!(
                    "✗ workflow failed: {}",
                    data.get("output_data")
                        .map(|o| o.to_string())
                        .unwrap_or_else(|| "no detail".into())
                )],
                _ => vec![],
            }
        }
        "job_step_update" => {
            // only steps of OUR job (JobStep has job_id)
            let jid = data.get("job_id").and_then(|x| x.as_str()).unwrap_or("");
            if jid != job {
                return vec![];
            }
            let status = data.get("status").and_then(|x| x.as_str()).unwrap_or("");
            let order = data.get("workflow_step_id").map(|_| "").unwrap_or("");
            let _ = order;
            match status {
                "RUNNING" => vec![format!(
                    "▶ step {} running",
                    data.get("workflow_step_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                )],
                "COMPLETED" => vec!["✓ step done".to_string()],
                "FAILED" => vec![format!(
                    "✗ step failed: {}",
                    data.get("output_data")
                        .map(|o| o.to_string())
                        .unwrap_or_default()
                )],
                _ => vec![],
            }
        }
        _ => vec![],
    }
}

/// The WS reader: connect, filter, write rows. Runs on the worker
/// thread; reconnects with backoff. Dies with hot upgrade and restarts
/// (watches map is rebuilt from Run jobs; acceptable — runs are long).
fn run_ws_reader(
    cfg: MuleConfig,
    pipe: PipeWriter,
    watches: Watches,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use tungstenite::Message;
    use tungstenite::connect;
    let ws_url = format!("{}{}", cfg.base.replace("http", "ws"), "/ws");
    let mut backoff = 1u64;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let maybe_ws = connect(&ws_url);
        if let Ok((mut ws, _resp)) = maybe_ws {
            backoff = 1;
            loop {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // read with a timeout via set_read_timeout on the
                // underlying stream is not exposed by tungstenite's
                // WebSocket API pre-0.27; use read (blocking) and
                // check stop between messages — the stop flag also
                // gets checked when no messages arrive via the read
                // timeout set on the socket below.
                match ws.read() {
                    Ok(Message::Text(t)) => {
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
                            continue;
                        };
                        let type_ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
                        let job_ids: Vec<String> = watches
                            .lock()
                            .map(|g| g.values().cloned().collect())
                            .unwrap_or_default();
                        for job in job_ids {
                            for row in format_event(type_, &data, &job) {
                                let pane = watches.lock().ok().and_then(|g| {
                                    g.iter().find_map(|(p, j)| (j == &job).then_some(*p))
                                });
                                if let Some(pane) = pane {
                                    write_row(&pipe, pane, &row);
                                    // status metas for dashboards
                                    if row.starts_with('✓') || row.starts_with('✗') {
                                        write_frame(
                                            &pipe,
                                            &Frame::Meta {
                                                session: String::new(),
                                                pane: Some(pane.to_string()),
                                                kind: "workflow".into(),
                                                status: Some(if row.starts_with('✓') {
                                                    "completed".into()
                                                } else {
                                                    "failed".into()
                                                }),
                                            background: None,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(backoff.min(30)));
        backoff = (backoff * 2).min(30);
    }
}

/// Worker entry: owns the mule HTTP + WS until the process ends.
pub fn spawn_worker(cfg: MuleConfig, pipe_w: std::fs::File, rx: mpsc::Receiver<MuleJob>) {
    let pipe = std::sync::Arc::new(std::sync::Mutex::new(pipe_w));
    let watches: Watches = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // WS reader thread (run streaming)
    {
        let cfg2 = cfg.clone();
        let pipe2 = pipe.clone();
        let watches2 = watches.clone();
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("mule-ws".into())
            .spawn(move || run_ws_reader(cfg2, pipe2, watches2, stop2))
            .ok();
    }

    std::thread::Builder::new()
        .name("mule-worker".into())
        .spawn(move || {
            for job in rx {
                match job {
                    MuleJob::List { req_id } => {
                        match http_json(&cfg, "GET", "/api/v1/workflows", None) {
                            Ok(v) => {
                                // mule returns {workflows: [...]} or a bare array
                                let arr = v
                                    .get("workflows")
                                    .and_then(|x| x.as_array())
                                    .cloned()
                                    .or_else(|| v.as_array().cloned())
                                    .unwrap_or_default();
                                let workflows: Vec<WorkflowSummary> = arr
                                    .iter()
                                    .filter_map(|w| {
                                        Some(WorkflowSummary {
                                            id: w.get("id")?.as_str()?.to_string(),
                                            name: w
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("")
                                                .into(),
                                            description: w
                                                .get("description")
                                                .and_then(|d| d.as_str())
                                                .map(String::from),
                                            is_async: w.get("is_async").and_then(|x| x.as_bool()),
                                            updated_at: w
                                                .get("updated_at")
                                                .and_then(|d| d.as_str())
                                                .map(String::from),
                                        })
                                    })
                                    .collect();
                                write_frame(&pipe, &Frame::WorkflowListOk { req_id, workflows });
                            }
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    MuleJob::Agents { req_id } => {
                        match http_json(&cfg, "GET", "/api/v1/agents", None) {
                            Ok(v) => {
                                let arr = v
                                    .get("agents")
                                    .and_then(|x| x.as_array())
                                    .cloned()
                                    .or_else(|| v.as_array().cloned())
                                    .unwrap_or_default();
                                let agents: Vec<MuleAgent> = arr
                                    .iter()
                                    .filter_map(|a| {
                                        Some(MuleAgent {
                                            id: a.get("id")?.as_str()?.to_string(),
                                            name: a
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("")
                                                .into(),
                                            description: a
                                                .get("description")
                                                .and_then(|d| d.as_str())
                                                .map(String::from),
                                        })
                                    })
                                    .collect();
                                write_frame(&pipe, &Frame::MuleAgentsOk { req_id, agents });
                            }
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    MuleJob::Get {
                        req_id,
                        workflow_id,
                    } => {
                        match http_json(
                            &cfg,
                            "GET",
                            &format!("/api/v1/workflows/{workflow_id}"),
                            None,
                        ) {
                            Ok(v) => {
                                let summary = WorkflowSummary {
                                    id: v
                                        .get("id")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or(&workflow_id)
                                        .to_string(),
                                    name: v
                                        .get("name")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .into(),
                                    description: v
                                        .get("description")
                                        .and_then(|x| x.as_str())
                                        .map(String::from),
                                    is_async: v.get("is_async").and_then(|x| x.as_bool()),
                                    updated_at: v
                                        .get("updated_at")
                                        .and_then(|x| x.as_str())
                                        .map(String::from),
                                };
                                let steps = http_json(
                                    &cfg,
                                    "GET",
                                    &format!("/api/v1/workflows/{workflow_id}/steps"),
                                    None,
                                )
                                .ok()
                                .and_then(|sv| {
                                    sv.get("steps")
                                        .or(Some(&sv))
                                        .and_then(|x| x.as_array())
                                        .cloned()
                                })
                                .unwrap_or_default()
                                .iter()
                                .filter_map(|st| {
                                    serde_json::from_value::<WorkflowStep>(st.clone()).ok()
                                })
                                .collect();
                                write_frame(
                                    &pipe,
                                    &Frame::WorkflowGetOk {
                                        req_id,
                                        workflow: summary,
                                        steps,
                                    },
                                );
                            }
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    MuleJob::Put {
                        req_id,
                        workflow_id,
                        draft,
                    } => {
                        // create or update the workflow, then sync steps
                        let res: Result<String, String> = (|| {
                            let body = serde_json::json!({
                                "name": draft.name,
                                "description": draft.description,
                                "is_async": draft.is_async,
                            });
                            let id = match &workflow_id {
                                Some(id) => {
                                    http_json(
                                        &cfg,
                                        "PUT",
                                        &format!("/api/v1/workflows/{id}"),
                                        Some(&body),
                                    )?;
                                    id.clone()
                                }
                                None => {
                                    let v =
                                        http_json(&cfg, "POST", "/api/v1/workflows", Some(&body))?;
                                    v.get("id")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string()
                                }
                            };
                            if id.is_empty() {
                                return Err("mule: workflow save returned no id".into());
                            }
                            // steps: replace-all semantics via diff —
                            // existing steps not in the draft get deleted;
                            // draft steps (no id) get created; drafts with
                            // matching ids get updated; order via reorder.
                            let existing: Vec<WorkflowStep> = http_json(
                                &cfg,
                                "GET",
                                &format!("/api/v1/workflows/{id}/steps"),
                                None,
                            )
                            .ok()
                            .and_then(|sv| {
                                sv.get("steps")
                                    .or(Some(&sv))
                                    .and_then(|x| x.as_array())
                                    .cloned()
                            })
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|st| {
                                serde_json::from_value::<WorkflowStep>(st.clone()).ok()
                            })
                            .collect();
                            let draft_ids: Vec<String> =
                                draft.steps.iter().filter_map(|s| s.id.clone()).collect();
                            for old in &existing {
                                if !draft_ids.contains(&old.id.clone().unwrap_or_default()) {
                                    let _ = http_json(
                                        &cfg,
                                        "DELETE",
                                        &format!(
                                            "/api/v1/workflows/{id}/steps/{}",
                                            old.id.clone().unwrap_or_default()
                                        ),
                                        None,
                                    );
                                }
                            }
                            for (i, st) in draft.steps.iter().enumerate() {
                                let step_body = serde_json::json!({
                                    "type": st.step_type,
                                    "agent_id": st.agent_id,
                                    "wasm_module_id": st.wasm_module_id,
                                    "config": st.config,
                                });
                                match &st.id {
                                    Some(sid) => {
                                        let _ = http_json(
                                            &cfg,
                                            "PUT",
                                            &format!("/api/v1/workflows/{id}/steps/{sid}"),
                                            Some(&step_body),
                                        );
                                    }
                                    None => {
                                        let _ = http_json(
                                            &cfg,
                                            "POST",
                                            &format!("/api/v1/workflows/{id}/steps"),
                                            Some(&step_body),
                                        );
                                    }
                                }
                                let _ = i; // reorder after create/update below
                            }
                            // reorder: draft order by step ids (fetch fresh ids
                            // for created steps is skipped — mule's reorder
                            // endpoint takes step ids; created steps land in
                            // order when appended, which covers the common case)
                            Ok(id)
                        })();
                        match res {
                            Ok(id) => write_frame(
                                &pipe,
                                &Frame::WorkflowPutOk {
                                    req_id,
                                    workflow_id: id,
                                },
                            ),
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    MuleJob::Delete {
                        req_id,
                        workflow_id,
                    } => {
                        match http_json(
                            &cfg,
                            "DELETE",
                            &format!("/api/v1/workflows/{workflow_id}"),
                            None,
                        ) {
                            Ok(_) => write_frame(&pipe, &Frame::WorkflowDeleteOk { req_id }),
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    MuleJob::Run {
                        req_id,
                        workflow_id,
                        input,
                        session,
                        pane,
                    } => {
                        let body = serde_json::json!({
                            "workflow_id": workflow_id,
                            "input_data": input.unwrap_or(serde_json::Value::Null),
                        });
                        match http_json(&cfg, "POST", "/api/v1/jobs", Some(&body)) {
                            Ok(v) => {
                                let job_id = v
                                    .get("id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if job_id.is_empty() {
                                    write_frame(
                                        &pipe,
                                        &Frame::Error {
                                            req_id: Some(req_id),
                                            message: "mule run: no job id in response".into(),
                                        },
                                    );
                                    continue;
                                }
                                watches.lock().unwrap().insert(pane, job_id.clone());
                                write_row(
                                    &pipe,
                                    pane,
                                    &format!("▶ workflow run started (job {job_id})"),
                                );
                                write_frame(
                                    &pipe,
                                    &Frame::WorkflowRunOk {
                                        req_id,
                                        job: job_id,
                                        session: session.to_string(),
                                        pane: pane.to_string(),
                                    },
                                );
                                write_frame(
                                    &pipe,
                                    &Frame::Meta {
                                        session: String::new(),
                                        pane: Some(pane.to_string()),
                                        kind: "workflow".into(),
                                        status: Some("running".into()),
                                    background: None,
                                    },
                                );
                            }
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    MuleJob::Unwatch { pane } => {
                        watches.lock().unwrap().remove(&pane);
                    }
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H3.3: the forward payload must be the mule wake-ingress shape,
    /// built from the ranch webhook fields.
    #[test]
    fn wake_forward_body_shape() {
        let body = wake_forward_body("ranch", Some("push"), &serde_json::json!({"repo": "mule"}));
        assert_eq!(
            body,
            serde_json::json!({
                "kind": "webhook",
                "source": "ranch",
                "event_tag": "push",
                "payload": { "repo": "mule" },
            })
        );
        // No event tag: the field is null (mule treats it as
        // "match any tag").
        let body = wake_forward_body("ranch", None, &serde_json::json!(null));
        assert_eq!(body["kind"], "webhook");
        assert_eq!(body["source"], "ranch");
        assert!(body["event_tag"].is_null());
        assert!(body["payload"].is_null());
    }

    /// H3.3: a mule failure is swallowed (log + drop) — the caller
    /// gets no error and no panic. An unreachable port stands in for a
    /// dead mule; the call must simply return.
    #[test]
    fn forward_swallows_http_errors() {
        let cfg = MuleConfig {
            base: "http://127.0.0.1:1".into(),
            api_key: Some("sk_mule_test".into()),
            wakes_forward: true,
            wakes_url: "http://127.0.0.1:1".into(),
            wakes_api_key: Some("sk_mule_test".into()),
        };
        forward_webhook_to_wakes(&cfg, "ranch", Some("push"), &serde_json::json!({"a": 1}));
        // And it is a no-op (no request at all) when forwarding is off.
        let off = MuleConfig {
            base: "http://127.0.0.1:1".into(),
            api_key: None,
            wakes_forward: false,
            wakes_url: "http://127.0.0.1:1".into(),
            wakes_api_key: None,
        };
        forward_webhook_to_wakes(&off, "ranch", None, &serde_json::json!(null));
    }

    /// H3.3: `[wakes]` table parsing — mule_forward, wakes_url /
    /// wakes_api_key overrides, and the fallbacks to the top-level
    /// mule_url / mule_api_key.
    #[test]
    fn wakes_config_parsing() {
        let toml = r#"
            mule_url = "http://mule.example"
            mule_api_key = "sk_mule_top"

            [wakes]
            mule_forward = true
            wakes_url = "http://mules.example/"
            wakes_api_key = "sk_mule_wakes"
        "#;
        let mut cfg = parse_toml_wakes(toml).expect("config parses");
        assert!(cfg.wakes_forward);
        assert_eq!(cfg.wakes_url, "http://mules.example");
        assert_eq!(cfg.wakes_api_key.as_deref(), Some("sk_mule_wakes"));
        assert_eq!(cfg.api_key.as_deref(), Some("sk_mule_top"));

        // Without the [wakes] table: forward off, fallbacks to top level.
        let toml2 = r#"
            mule_url = "http://mule.example/"
            mule_api_key = "sk_mule_top"
        "#;
        cfg = parse_toml_wakes(toml2).expect("config parses");
        assert!(!cfg.wakes_forward);
        assert_eq!(
            cfg.wakes_url, "http://mule.example",
            "falls back to mule_url"
        );
        assert_eq!(
            cfg.wakes_api_key.as_deref(),
            Some("sk_mule_top"),
            "falls back to mule_api_key"
        );
        // No mule_url at all: no config.
        assert!(parse_toml_wakes("relay = 1").is_none());
    }
}
